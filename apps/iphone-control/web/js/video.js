let firstFrameTimer = 0;
function feedError() {
  if (S.stream === 'error') return;
  cancelGestures(); ++feedEpoch;
  clearTimeout(firstFrameTimer); S.stream = 'error'; S.rtt = null;
  stopMjpegTurn(); stopVideo(); cv.hidden = true; live.style.visibility = 'hidden'; live.removeAttribute('src');
  marks.replaceChildren(); hoverDot = null; S.elements = []; S.elState = 'idle'; renderElements();
  applyZoom(); showStageNote(); renderFeed(); renderChips();
  const epoch = feedEpoch;
  retryTimer = setTimeout(async () => {
    await loadInfo();
    if (epoch === feedEpoch) connect();
  }, 3000);
}
function refreshFrame(reoriented = false) {
  if (S.mode !== 'live' || S.stream === 'error' || (V.on && !V.got) || !picW() || !picH()) return;
  if (S.stream === 'ok' && !reoriented && picW() === frameWidth && picH() === frameHeight) return;
  if (S.stream === 'ok') {
    cancelGestures(); marks.replaceChildren(); hoverDot = null; S.elements = []; S.elState = 'idle';
    loadInfo();
  }
  clearTimeout(firstFrameTimer);
  S.stream = 'ok'; stageMsg.hidden = true; picEl().style.visibility = '';
  frameWidth = picW(); frameHeight = picH();
  setRatio(frameWidth, frameHeight); syncMarksBox(); renderElements(); renderFeed(); renderChips();
}
/* MJPEG 帧是竖的而界面是横的：img 照常接收（透明），每个动画帧把它转正画到 canvas 上。iPhone 的截图带 EXIF 方向，
 * 浏览器显示 img 时已经按它转正（naturalWidth 已是横的），这里就不转；没有方向标记的帧才用得上 */
function mjpegTurn() {
  const sw = live.naturalWidth, sh = live.naturalHeight, r = wdaTurn(sw, sh);
  if (!r) { stopMjpegTurn(); return; }
  M.on = true; cv.width = sh; cv.height = sw; cv.hidden = false; live.style.opacity = '0';
  cancelAnimationFrame(M.raf);
  const draw = () => {
    if (!M.on) return;
    cvx.save(); cvx.translate(sh / 2, sw / 2); cvx.rotate(r * Math.PI / 180);
    cvx.drawImage(live, -sw / 2, -sh / 2, sw, sh); cvx.restore();
    M.raf = requestAnimationFrame(draw);
  };
  draw();
}
function stopMjpegTurn() {
  if (!M.on) return;
  M.on = false; cancelAnimationFrame(M.raf); live.style.opacity = '';
  if (!V.on) cv.hidden = true;
}
/* 画面流的编号。不用 crypto.randomUUID：它只在安全上下文（HTTPS 或 localhost）里有，从局域网地址（viewer --lan）打开时没有 */
function newFeedId() {
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), x => x.toString(16).padStart(2, '0')).join('');
}
function connect() {
  stopMjpegTurn();
  cancelGestures(); const epoch = ++feedEpoch;
  clearTimeout(firstFrameTimer); clearTimeout(retryTimer);
  stopVideo(); cv.hidden = true;
  feedId = newFeedId();
  S.stream = 'connecting'; frameWidth = frameHeight = 0;
  applyZoom();
  stageMsg.textContent = tr('正在连接画面…'); delete stageMsg.dataset.note; stageMsg.hidden = false;
  const next = live.cloneNode(false); next.removeAttribute('src'); next.hidden = false; next.style.visibility = 'hidden';
  live.replaceWith(next); live.removeAttribute('src'); live = next;
  if (!S.udid) { feedError(); return; }
  const q = 'udid=' + encodeURIComponent(S.udid) + '&feed=' + encodeURIComponent(feedId);
  const dev = S.deviceVideo && !V.noDevice;
  if ((dev || (S.video && !V.off)) && 'VideoDecoder' in window) {
    V.on = true; next.hidden = true; cv.style.visibility = 'hidden'; cv.hidden = false;
    startVideo(q, dev);
  } else {
    next.onload = () => { if (epoch === feedEpoch) { mjpegTurn(); refreshFrame(); } };
    next.onerror = () => { if (epoch === feedEpoch) feedError(); };
    next.src = '/stream?' + q;
  }
  firstFrameTimer = setTimeout(() => {
    if (epoch !== feedEpoch) return;
    if (V.on && !V.got) { giveUpVideo(dev, false); connect(); }
    else feedError();
  }, 9000);
  renderFeed(); renderChips();
}
/* ---------- H.264 画面：服务端用 ffmpeg 编码（硬件优先），这里用 WebCodecs 解码（浏览器自己选硬件）画到 canvas。
 * 消息：1 字节类型 + 4 字节长度 + 内容。0 是解码配置（JSON），1 关键帧，2 其他帧。
 * 这一页从没出过画面就失败（没有编码器、浏览器解不了）就改用 MJPEG，之后不再试 ---------- */
/* 放弃这一路画面，改用下一种。服务端说走不通（404：没有编码器、手机不支持）就不再试；只是第一帧迟迟不来
 * （viewer 或守护进程正在重启、编码器正在启动）的，过一阵再试：之前这样一次就永远停在 MJPEG。最多再试几次，
 * 真走不通的不会一直断画面 */
const VIDEO_RETRY_MS = [30000, 60000, 120000];
function giveUpVideo(dev, permanent) {
  if (dev) V.noDevice = true; else V.off = true;
  if (permanent) return;
  const n = dev ? (V.devRetries = (V.devRetries || 0) + 1) : (V.offRetries = (V.offRetries || 0) + 1);
  if (n > VIDEO_RETRY_MS.length) return;
  setTimeout(() => {
    if (dev) V.noDevice = false; else V.off = false;
    if (S.mode === 'live') connect();
  }, VIDEO_RETRY_MS[n - 1]);
}
function stopVideo() {
  if (V.ctrl) V.ctrl.abort();
  V.ctrl = null; V.on = false; V.got = false; V.rate = null; V.stream = ''; V.recv = 0; V.acked = 0; V.acking = 0; V.reported = 0;
  V.device = false; V.crop = false; V.rot = null; V.frameRotation = null; V.bytes = 0; V.codecName = '';
  if (V.dec && V.dec.state !== 'closed') { try { V.dec.close(); } catch { /* 已经关了 */ } }
  V.dec = null;
}
async function startVideo(q, dev) {
  const ctrl = new AbortController(); V.ctrl = ctrl;
  const ended = permanent => {
    if (V.ctrl !== ctrl) return;   // 已经换了一条连接
    const got = V.got;
    stopVideo();
    // 这一路从没出过画面就走不通：手机的视频走不通改用本机编码，本机编码也不行改用 MJPEG，之后不再试
    if (permanent && !got) { giveUpVideo(dev, true); connect(); return; }
    feedError();
  };
  const onFrame = f => {
    if (V.ctrl !== ctrl) { f.close(); return; }
    // 手机的视频是竖放的帧缓冲，宽高补到 16 的倍数（1320×2868 编成 1328×2896）：裁掉补的边，再按界面方向转正
    let sw = f.displayWidth, sh = f.displayHeight;
    if (V.crop && S.size) {
      const pw = Math.min(S.size.w, S.size.h), ph = Math.max(S.size.w, S.size.h), k = Math.round(sw / pw) || 1;
      sw = Math.min(sw, Math.round(pw * k)); sh = Math.min(sh, Math.round(ph * k));
    }
    const r = V.crop ? (V.rot ?? S.display?.rotation ?? 0) : wdaTurn(sw, sh), turned = r === 90 || r === 270;
    const W = turned ? sh : sw, H = turned ? sw : sh;
    const resized = cv.width !== W || cv.height !== H;
    if (resized) { cv.width = W; cv.height = H; }
    if (r) {
      cvx.save(); cvx.translate(W / 2, H / 2); cvx.rotate(r * Math.PI / 180);  // r：竖放的顶边顺时针转过的角度，画面同向转回来
      cvx.drawImage(f, 0, 0, sw, sh, -sw / 2, -sh / 2, sw, sh); cvx.restore();
    } else cvx.drawImage(f, 0, 0, sw, sh, 0, 0, sw, sh);
    f.close();
    const reoriented = V.frameRotation !== r;
    V.frameRotation = r; V.got = true;
    refreshFrame(reoriented);
  };
  try {
    const r = await fetch(`/video?${q}${dev ? '&src=device' : ''}${lowBandwidthOn() ? '&low=1' : ''}`, { signal: ctrl.signal, headers: { 'X-Ictl-Lang': LANG } });
    if (loggedOut(r)) return;
    if (!r.ok || !r.body) return ended(r.status === 404);
    const rd = r.body.getReader();
    let buf = new Uint8Array(0);
    for (;;) {
      const { value, done } = await rd.read();
      if (V.ctrl !== ctrl) return;
      if (done) return ended(!V.got);
      V.bytes += value.length; V.recv += value.length;
      const n = new Uint8Array(buf.length + value.length); n.set(buf); n.set(value, buf.length); buf = n;
      while (buf.length >= 5) {
        const len = ((buf[1] << 24) | (buf[2] << 16) | (buf[3] << 8) | buf[4]) >>> 0;
        if (buf.length < 5 + len) break;
        const kind = buf[0], body = buf.slice(5, 5 + len);
        buf = buf.slice(5 + len);
        if (kind === 0) {
          const c = JSON.parse(new TextDecoder().decode(body));
          // 手机的 HEVC 是 Annex-B（参数集在关键帧里），不带 description
          const config = c.annexb ? { codec: c.codec, optimizeForLatency: true }
            : { codec: c.codec, description: Uint8Array.from(c.description.match(/../g) || [], h => parseInt(h, 16)), optimizeForLatency: true };
          V.device = !!c.annexb; V.crop = !!c.annexb; V.codecName = /^(hvc|hev)/.test(c.codec) ? 'HEVC' : 'H.264';
          let ok = false;
          try { ok = (await VideoDecoder.isConfigSupported(config)).supported; } catch { /* 当作不支持 */ }
          if (!ok) return ended(true);
          if (V.ctrl !== ctrl) return;
          if (!V.dec) V.dec = new VideoDecoder({ output: onFrame, error: () => ended(!V.got) });
          V.dec.configure(config); V.label = c.encoder || ''; V.stream = c.stream || '';
        } else if (kind === 3) {
          // 手机的视频：界面转了多少度（竖放的顶边顺时针转过去的角度），下一帧按它转正
          try { const i = JSON.parse(new TextDecoder().decode(body)); if (typeof i.rotation === 'number') V.rot = i.rotation; } catch { /* 忽略 */ }
        } else if (V.dec && V.dec.state === 'configured') {
          V.dec.decode(new EncodedVideoChunk({ type: kind === 1 ? 'key' : 'delta', timestamp: V.ts += 40000, data: body }));
        }
      }
    }
  } catch {
    if (!ctrl.signal.aborted) ended(!V.got);
  }
}
/* 每秒算一次 H.264 画面的流量，显示在画面下方 */
setInterval(() => { if (V.on) { V.rate = V.bytes; V.bytes = 0; renderFeed(); } }, 1000);
/* 告诉服务端读到了多少：链路慢、隧道里堆了数据时，它据此少编几帧，画面不会越来越滞后。
 * 服务端收到才算报过，丢了的下一轮再报（不然它会一直等这次回报）。最多两个回报同时在路上：
 * Wi-Fi 上一次回报偶尔要几百毫秒，下一个不必等它，先到的就先算 */
setInterval(() => {
  if (!V.on || !V.stream || V.recv === V.acked || V.acking >= 2 || V.recv === V.reported) return;
  const stream = V.stream, bytes = V.recv;
  V.acking = (V.acking || 0) + 1; V.reported = bytes;
  fetch('/api/video/ack', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ stream, bytes }) })
    .then(r => { if (r.ok && V.stream === stream) V.acked = Math.max(V.acked, bytes); else if (V.stream === stream) V.reported = V.acked; })
    .catch(() => { if (V.stream === stream) V.reported = V.acked; })
    .finally(() => { if (V.stream === stream) V.acking = Math.max(0, V.acking - 1); });
}, 100);
function renderFeed() {
  const dot = $('#feed-dot'), txt = $('#feed-text');
  dot.className = 'feed-dot';
  if (S.mode === 'demo') { txt.textContent = tr('演示画面 · 本地模拟'); return; }
  if (S.mode === 'detecting' || S.stream === 'connecting') { dot.classList.add('wait'); txt.textContent = tr('正在连接画面…'); return; }
  if (frame.hasAttribute('data-absent')) { dot.classList.add('wait'); txt.textContent = tr('等待设备连接'); return; }
  if (S.stream === 'error') { dot.classList.add('off'); txt.textContent = tr('画面断开 · 自动重试中'); return; }
  if (!dims()) { dot.classList.add('wait'); txt.textContent = tr('画面已连接 · 等待屏幕尺寸和方向，仅可观看'); return; }
  const route = S.dstatus?.route === 'hid' ? tr('HID 后备') : S.dstatus?.route === 'native' ? tr('原生') : 'WDA';
  const codec = V.on ? ` · ${V.codecName || 'H.264'}${V.label ? ` ${V.label}` : ''}${V.rate != null ? ` ${Math.round(V.rate / 1024)} KB/s` : ''}` : ' · MJPEG';
  const low = V.on && lowBandwidthOn() ? ` · ${tr('弱网模式')}` : '';
  txt.textContent = `${tr('实时画面')} · ${route}${codec}${low}${S.rtt != null ? ` · ${tr('操作往返')} ${S.rtt} ms` : ''}`;
}

/* ================================================================
 * 设备：状态、切换
 * ================================================================ */
