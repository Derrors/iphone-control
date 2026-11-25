function dims() {
  if (S.mode === 'demo') return { W: DW(), H: DH() };
  if (!S.display?.id || S.switching || S.stream !== 'ok' || !picH()) return null;
  const { width, height, rotation } = S.display;
  if (!(width > 0 && height > 0) || ![0, 90, 180, 270].includes(rotation)) return null;
  if (V.device && V.frameRotation !== rotation) return null;
  const land = rotation === 90 || rotation === 270;
  const W = land ? height : width, H = land ? width : height;
  return Math.abs(picW() / picH() / (W / H) - 1) <= 0.01 ? { W, H } : null;
}
function setRatio(w, h) {
  stage.style.aspectRatio = `${w} / ${h}`; frame.style.setProperty('--ratio', String(w / h));
  frame.dataset.kind = S.mode === 'demo' ? demoDev.kind : S.family;
  applyZoom();
}
function applyDemoGeometry() {
  demoEl.style.setProperty('--dw', DW() + 'px'); demoEl.style.setProperty('--dh', DH() + 'px');
  setRatio(DW(), DH()); syncMarksBox();
  requestAnimationFrame(() => demoEl.style.setProperty('--k', String(stage.clientWidth / DW())));
}
function syncMarksBox() { const d = dims(); if (d) marks.setAttribute('viewBox', `0 0 ${d.W} ${d.H}`); }
function pictureRect() {
  const r = stage.getBoundingClientRect();
  if (S.mode !== 'live') return r;
  const k = Math.min(r.width / picW(), r.height / picH());
  const width = k * picW(), height = k * picH();
  return { left: r.left + (r.width - width) / 2, top: r.top + (r.height - height) / 2, width, height };
}
function toPoints(cx, cy, clamp = false) {
  const d = dims(); if (!d) return null;
  const r = pictureRect(); if (!r.width || !r.height) return null;
  const x = (cx - r.left) / r.width, y = (cy - r.top) / r.height;
  if (!clamp && (x < 0 || x > 1 || y < 0 || y > 1)) return null;
  return { x: round1(Math.min(1, Math.max(0, x)) * d.W), y: round1(Math.min(1, Math.max(0, y)) * d.H) };
}
new ResizeObserver(() => { demoEl.style.setProperty('--k', String(stage.clientWidth / DW())); }).observe(stage);

const SVGNS = 'http://www.w3.org/2000/svg';
function svgEl(tag, attrs) { const el = document.createElementNS(SVGNS, tag); for (const k in attrs) el.setAttribute(k, attrs[k]); return el; }
function shape(who, p, r) {
  return who === 'ai'
    ? svgEl('rect', { x: p.x - r, y: p.y - r, width: r * 2, height: r * 2, transform: `rotate(45 ${p.x} ${p.y})`, class: 'mk-shape' })
    : svgEl('circle', { cx: p.x, cy: p.y, r, class: 'mk-shape' });
}
function drawMark(kind, who, p, p2) {
  // 只标 AI 的操作；自己的操作看画面就够了
  if (!p || who !== 'ai') return;
  const g = svgEl('g', { class: `mk mk-${who}` });
  if (kind === 'swipe' && p2) {
    g.append(svgEl('line', { x1: p.x, y1: p.y, x2: p2.x, y2: p2.y, class: 'mk-line' }), svgEl('circle', { cx: p.x, cy: p.y, r: 7, class: 'mk-dot' }), shape(who, p2, 12));
  } else {
    g.append(shape(who, p, kind === 'long_press' ? 24 : 16));
    if (kind === 'long_press') g.append(svgEl('circle', { cx: p.x, cy: p.y, r: 36, class: 'mk-ring' }));
  }
  if (who === 'ai') { const t = svgEl('text', { x: p.x + 26, y: p.y - 22, class: 'mk-tag' }); t.textContent = 'AI'; g.append(t); }
  marks.append(g);
  setTimeout(() => g.classList.add('out'), 1000);
  setTimeout(() => g.remove(), 1250);
}
let hlRect = null;
function highlight(el) {
  hlRect?.remove(); hlRect = null;
  if (!el || !el.width || !dims()) return;
  hlRect = svgEl('rect', { x: el.x, y: el.y, width: el.width, height: el.height, rx: 6, class: 'mk-hl' });
  marks.append(hlRect);
}

/* ================================================================
 * 请求
 * ================================================================ */
/* 登录过期或令牌换了（viewer token --new）：刷新，服务端回登录页。只在真的接上了 viewer 时（演示模式没有登录） */
function loggedOut(r) {
  if (r.status !== 401 || !document.documentElement.hasAttribute('data-viewer')) return false;
  if (!loggedOut.done) { loggedOut.done = true; location.reload(); }
  return true;
}
async function api(path, body, device = S.udid) {
  const opt = body === undefined ? {} : { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) };
  opt.headers = { ...(opt.headers || {}), 'X-Ictl-Lang': LANG };
  // 操作只发给本页显示的那台；守护进程已换到别的设备时会被拒绝，不会点到用户没在看的手机上
  if (device && !path.startsWith('/api/updates')) opt.headers['X-Ictl-Device'] = device;
  const r = await fetch(path, opt);
  if (loggedOut(r)) throw new Error(tr('登录已过期'));
  if (!r.ok) {
    let msg = '';
    try { msg = (await r.json()).message || ''; } catch { /* 不是 JSON */ }
    if (r.status === 404 && !msg) msg = `viewer ${tr('没有这个接口（')}${path.split('?')[0]}${tr('）')}`;
    throw new Error(msg || `${tr('请求失败（')}HTTP ${r.status}${tr('）')}`);
  }
  return r.json();
}
async function act(path, body, quiet) {
  if (S.readOnly) { show(tr('只读登录：只能看，不能操作'), 'warn'); return { ok: false }; }
  if (S.mode === 'live' && path !== '/api/select' && (S.switching || !S.udid)) return { ok: false };
  const t0 = performance.now(), epoch = feedEpoch;
  try {
    const v = await api(path, body);
    if (epoch !== feedEpoch) return v;
    S.rtt = Math.round(performance.now() - t0); renderFeed();
    // 画面上的手势、底栏的按键和输入：成功时不弹提示，画面本身就是结果；失败照样提示
    if (!quiet || !v.ok) show(v.message || (v.ok ? tr('完成') : tr('失败')), v.ok ? null : 'err');
    if (v.ok) refreshStatusSoon();
    // 失败可能是设备已在别处换掉（操作带着旧 UDID 被拒）：马上看一眼，换了就跟过去，不用等 10 秒的轮询
    else if (S.mode === 'live') loadInfo();
    return v;
  } catch (e) { show(String(e.message || e), 'err'); return { ok: false, message: String(e.message || e) }; }
}

/* ================================================================
 * 手动操作
 * ================================================================ */
let down = null;
function nudgeGuard() {
  const g = $('#guard');
  g.classList.remove('nudge'); void g.offsetWidth; g.classList.add('nudge');
  const t = activeTask();
  show(t && t.status === 'waiting' ? tr('AI 在等你确认一步操作，确认前画面保持锁定。') : tr('AI 正在操作这台设备。要自己操作，先点「接管」。'), 'warn');
}
/* 画面外的点夹到屏幕边上（拖动时手指不会离开屏幕） */
function pointsPerPx() {
  const d = dims();
  return d ? d.W / pictureRect().width : 1;
}

/* ---------- 实时手指（iOS 27 起，经 CoreDevice HID）：按下、移动、抬起各发一次，设备跟着鼠标动 ----------
 * 一次只有一个请求在路上：还没发出去的移动只留最新的一个，按下、抬起一定按顺序发。
 * 按住不动时每 100 毫秒再报一次当前位置，长按、拖动中途停顿都和真手指一样。 */
const T = { q: [], busy: false, on: false, last: null, keep: 0, warm: 0, fails: 0, gesture: 0 };
function touchSend(phase, p) {
  alertQuiet();
  const tail = T.q[T.q.length - 1];
  if (phase === 'move' && tail?.phase === 'move' && tail.gesture === T.gesture) tail.p = p;
  else T.q.push({ phase, p, gesture: T.gesture, view: S.display?.id, device: S.udid, epoch: feedEpoch });
  touchPump();
}
async function touchPump() {
  if (T.busy) return;
  T.busy = true;
  while (T.q.length) {
    const e = T.q.shift(), t0 = performance.now();
    let v;
    try { v = await api('/api/touch', { phase: e.phase, x: e.p ? e.p.x : 0, y: e.p ? e.p.y : 0, view: e.view }, e.device); }
    catch (err) { v = { ok: false, message: String(err.message || err) }; }
    if (!v.ok && e.phase === 'down') {
      // Don't send moves from a failed press. Keep its release in case the reply was lost.
      T.q = T.q.filter(next => next.gesture !== e.gesture || next.phase !== 'move');
    }
    if (e.epoch !== feedEpoch || e.gesture !== T.gesture) continue;
    if (e.phase === 'down' || e.phase === 'up') { S.rtt = Math.round(performance.now() - t0); renderFeed(); }
    if (v.ok) { if (e.phase === 'down') T.fails = 0; if (e.phase === 'up') refreshStatusSoon(); continue; }
    if (e.phase === 'prepare') continue;
    show(v.message || tr('操作失败'), 'err');
    if (e.phase === 'down') {
      fingerUp(T.last);
      if (v.retry_as_gesture !== true) {
        if (down?.gesture === e.gesture) { down = null; hideHoverDot(); }
        if (W.on && W.gesture === e.gesture) {
          W.on = false; W.pend = { x: 0, y: 0 };
          clearTimeout(W.idle); cancelAnimationFrame(W.raf);
        }
        loadInfo();
        continue;
      }
      if (down?.gesture === e.gesture) down.legacy = true;
      // 同一画面连续两次在按下前无法建立 HID 会话，后续改用整段手势。
      if (++T.fails >= 2) { S.liveTouch = false; renderFeed(); }
    }
  }
  T.busy = false;
}
function touchWarm() {
  if (!S.liveTouch || S.mode !== 'live' || !dims() || guarded() || Date.now() - T.warm < 20000) return;
  T.warm = Date.now(); touchSend('prepare', null);
}
function fingerDown(p) {
  ++T.gesture; T.on = true; T.last = p;
  touchSend('down', p);
  clearInterval(T.keep);
  T.keep = setInterval(() => { if (T.on && T.last) touchSend('move', T.last); }, 100);
  return T.gesture;
}
function fingerMove(p) { if (!T.on) return; T.last = p; touchSend('move', p); }
function fingerUp(p) { if (!T.on) return; T.on = false; clearInterval(T.keep); touchSend('up', p || T.last); T.last = null; }
stage.addEventListener('pointerenter', touchWarm);
// 画在画面上的系统弹窗：按下、移动、滚轮都不交给画面（否则会在卡片所在的位置再点一下手机），点击照常冒泡到事件委托
for (const t of ['pointerdown', 'pointermove', 'pointerup', 'pointercancel', 'wheel']) $('#sys-alert').addEventListener(t, ev => ev.stopPropagation());

stage.addEventListener('pointerdown', ev => {
  if (ev.button !== 0 || S.mode === 'detecting' || ev.target.closest('button, a, code')) return;
  if (guarded()) { nudgeGuard(); return; }
  const p = toPoints(ev.clientX, ev.clientY);
  if (!p || down) return;
  stage.setPointerCapture(ev.pointerId);
  wheelEnd(true);
  const liveFinger = S.mode === 'live' && S.liveTouch && !!p;
  down = { p, pointerId: ev.pointerId, t: performance.now(), cx: ev.clientX, cy: ev.clientY, live: liveFinger, legacy: false };
  if (liveFinger) down.gesture = fingerDown(p);
});
/* 鼠标悬停时跟随光标的小圆点，标出将要点到的位置；触屏、画面外、AI 操作时不显示 */
let hoverDot = null;
function moveHoverDot(p, ev) {
  if (!p || ev.pointerType === 'touch' || guarded()) { hideHoverDot(); return; }
  if (!hoverDot) {
    hoverDot = svgEl('g', { class: 'hover-dot' });
    const scale = svgEl('g', { class: 'dot-scale' });
    scale.append(svgEl('circle', { cx: 0, cy: 0, r: 9, class: 'ring' }), svgEl('circle', { cx: 0, cy: 0, r: 2.5, class: 'core' }));
    hoverDot.append(scale);
  }
  if (!hoverDot.isConnected) marks.append(hoverDot);
  hoverDot.removeAttribute('hidden');
  hoverDot.setAttribute('transform', `translate(${p.x} ${p.y})`);
  hoverDot.classList.toggle('pressed', !!down);
}
function hideHoverDot() { hoverDot?.setAttribute('hidden', ''); }
stage.addEventListener('pointerleave', hideHoverDot);
stage.addEventListener('pointermove', ev => {
  if (down && ev.pointerId !== down.pointerId) return;
  const p = toPoints(ev.clientX, ev.clientY);
  moveHoverDot(p, ev);
  // 鼠标停在画面上移动时也保持 HID 会话打开（空闲 30 秒会被关，按下时重开要约 0.5 秒）；touchWarm 自己限频到 20 秒一次
  if (!down) touchWarm();
  if (down && down.live && !down.legacy) {
    const pos = p || toPoints(ev.clientX, ev.clientY, true);
    if (pos) fingerMove(pos); else cancelGestures();
  }
});
function endPointer(ev, cancelled) {
  if (!down || ev.pointerId !== down.pointerId) return;
  const start = down; down = null;
  if (!dims()) { cancelGestures(); return; }
  hoverDot?.classList.remove('pressed');
  if (start.live && !start.legacy) { fingerUp(toPoints(ev.clientX, ev.clientY, true)); return; }
  if (cancelled) return;
  const end = toPoints(ev.clientX, ev.clientY), held = (performance.now() - start.t) / 1000;
  if (!start.p) { show(dims() ? tr('这个位置在画面外面') : tr('还不知道屏幕尺寸，稍后再试'), 'err'); return; }
  if (Math.hypot(ev.clientX - start.cx, ev.clientY - start.cy) < 8) userGesture(held < 0.5 ? 'tap' : 'long_press', start.p, null, held, start.cx, start.cy);
  else if (end) userGesture('swipe', start.p, end, held);
}
stage.addEventListener('pointerup', ev => endPointer(ev, false));
stage.addEventListener('pointercancel', ev => { endPointer(ev, true); hideHoverDot(); });

/* ---------- 滚轮、触控板：在画面上滚动就滚动设备上的页面，不用按住鼠标 ----------
 * 实时手指：在光标处按下（离屏幕边缘太近就挪进来，免得变成回主屏、下拉通知的手势），按滚动量移动，
 * 停下 160 毫秒后抬起；抬起前已经停住，不会再甩一段。触控板松手后的惯性滚动由浏览器继续发滚轮事件。
 * 手指走到屏幕边上就抬起、回到起点重新按下（按下会停住上一段的惯性）。
 * 没有实时手指时（iOS 27 以下）攒起滚动量，停下后发一次滑动。按住 Ctrl（触控板捏合）时不处理：捏合不缩放画面，也不滑手机（blockZoomWheel）。 */
const W = { on: false, start: null, cur: null, pend: { x: 0, y: 0 }, idle: 0, raf: 0 };
const EDGE = 90;
function wheelSafe(p) { const d = dims(); return { x: Math.min(d.W - 30, Math.max(30, p.x)), y: Math.min(d.H - EDGE, Math.max(EDGE, p.y)) }; }
stage.addEventListener('wheel', ev => {
  if (S.mode !== 'live' || ev.ctrlKey || ev.metaKey || down) return;
  const p = toPoints(ev.clientX, ev.clientY);
  if (!p) return;
  ev.preventDefault();
  if (guarded()) { nudgeGuard(); return; }
  const d = dims(), k = ev.deltaMode === 1 ? 16 : ev.deltaMode === 2 ? d.H * 0.8 / pointsPerPx() : 1;
  W.pend.x += ev.deltaX * k * pointsPerPx(); W.pend.y += ev.deltaY * k * pointsPerPx();
  if (!W.on) {
    W.on = true; W.start = wheelSafe(p); W.cur = { ...W.start };
    if (S.liveTouch) { W.gesture = fingerDown(W.cur); W.raf = requestAnimationFrame(wheelStep); }
  }
  clearTimeout(W.idle); W.idle = setTimeout(() => wheelEnd(false), 160);
}, { passive: false });
function wheelStep() {
  if (!W.on) return;
  const d = dims(); if (!d) { cancelGestures(); return; }
  // 一格滚轮分几帧走完，像手指拖过去
  const step = a => (Math.abs(a) < 1 ? a : a * 0.45);
  const sx = step(W.pend.x), sy = step(W.pend.y);
  W.pend.x -= sx; W.pend.y -= sy;
  if (sx || sy) {
    const next = { x: round1(W.cur.x - sx), y: round1(W.cur.y - sy) };
    if (next.x < 20 || next.x > d.W - 20 || next.y < 40 || next.y > d.H - 40) {
      fingerUp(W.cur); W.cur = { ...W.start }; W.gesture = fingerDown(W.cur);
    } else { W.cur = next; fingerMove(W.cur); }
  }
  W.raf = requestAnimationFrame(wheelStep);
}
function wheelEnd(now) {
  if (!W.on) return;
  if (!dims()) { cancelGestures(); return; }
  // 边移动边滑动时等手指把剩下的滚动量走完；整段手势（iOS 27 以下）没人消耗它，等下去就永远发不出滑动
  if (!now && S.liveTouch && (Math.abs(W.pend.x) > 1 || Math.abs(W.pend.y) > 1)) { W.idle = setTimeout(() => wheelEnd(false), 60); return; }
  clearTimeout(W.idle); cancelAnimationFrame(W.raf); W.on = false;
  if (S.liveTouch) { fingerUp(W.cur); }
  else {
    // 整段滑动：从光标处往滚动的反方向拖，最长 70% 屏高
    const d = dims(), max = d.H * 0.7;
    const dx = Math.max(-max, Math.min(max, W.pend.x)), dy = Math.max(-max, Math.min(max, W.pend.y));
    if (Math.abs(dx) > 4 || Math.abs(dy) > 4) {
      const a = W.start, b = { x: round1(Math.min(d.W - 10, Math.max(10, a.x - dx))), y: round1(Math.min(d.H - 10, Math.max(10, a.y - dy))) };
      act('/api/swipe', { view: S.display.id, x1: a.x, y1: a.y, x2: b.x, y2: b.y, seconds: 0.3 }, true);
    }
  }
  W.pend = { x: 0, y: 0 };
}

/* 整段手势（没有实时手指时）：松开后一次发出 */
function userGesture(kind, p, p2, held, cx, cy) {
  alertQuiet();
  if (S.mode === 'live') {
    if (kind === 'tap') act('/api/tap', { ...p, view: S.display.id }, true);
    else if (kind === 'long_press') act('/api/long_press', { ...p, view: S.display.id, seconds: round1(held) }, true);
    else act('/api/swipe', { view: S.display.id, x1: p.x, y1: p.y, x2: p2.x, y2: p2.y, seconds: Math.max(round1(held), 0.1) }, true);
    return;
  }
  const at = `(${p.x}, ${p.y})`;
  if (kind === 'tap') {
    const hit = document.elementsFromPoint(cx, cy).map(e => e.closest && e.closest('[data-act]')).find(e => e && demoEl.contains(e));
    show(`${tr('轻点')} ${at}${tr('：')}${hit ? demoAct(hit.dataset.act) : tr('这里没有可点的东西')}`);
  } else if (kind === 'long_press') show(`${tr('长按')} ${at} ${held.toFixed(1)} ${tr('秒：演示模式没有模拟长按菜单')}`);
  else {
    const up = p.y - p2.y;
    if ((D.screen === 'lock' && up > 80) || (p.y > DH() - 60 && up > 80)) show(`${tr('从')} ${at} ${tr('上滑：')}${demoKey('home')}`);
    else show(`${tr('滑动')} ${at} → (${p2.x}, ${p2.y})${tr('：演示画面不支持滚动')}`);
  }
}
$$('[data-key]').forEach(b => b.addEventListener('click', () => {
  if (guarded()) { nudgeGuard(); return; }
  if (S.mode === 'live') act('/api/button', { button: b.dataset.key }, true); else if (S.mode === 'demo') show(demoKey(b.dataset.key));
}));
async function typeText(submit) {
  if (guarded()) { nudgeGuard(); return; }
  const text = typeEl.value;
  if (!text && !submit) { show(tr('先输入要打到设备上的文字'), 'err'); typeEl.focus(); return; }
  if (S.mode === 'live') { if ((await act('/api/type', { text, submit }, true)).ok) typeEl.value = ''; return; }
  const r = demoType(text, submit);
  show(r.message, r.ok ? null : 'err');
  if (r.ok) typeEl.value = '';
}
$('#type-btn').addEventListener('click', () => typeText(false));
$('#type-submit').addEventListener('click', () => typeText(true));
typeEl.addEventListener('keydown', ev => { if (ev.key === 'Enter' && !ev.isComposing && ev.keyCode !== 229) { ev.preventDefault(); typeText(false); } });

/* ================================================================
 * 实时画面
 * ================================================================ */
function cancelGestures() {
  down = null; W.on = false; W.pend = { x:0, y:0 };
  clearTimeout(W.idle); cancelAnimationFrame(W.raf);
  T.q = T.q.filter(e => e.phase === 'up');
  fingerUp(T.last);
  hideHoverDot();
}
