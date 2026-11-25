const APPS = [
  { id: 'clock', name: tr('时钟'), bundle: 'com.apple.mobiletimer', glyph: tr('时'), bg: '#1c1c1e', fg: '#fff' },
  { id: 'calendar', name: tr('日历'), bundle: 'com.apple.mobilecal', glyph: String(new Date().getDate()), bg: '#fff', fg: '#ff3b30' },
  { id: 'photos', name: tr('照片'), bundle: 'com.apple.mobileslideshow', glyph: tr('照'), bg: '#fff', fg: '#ff9500' },
  { id: 'camera', name: tr('相机'), bundle: 'com.apple.camera', glyph: tr('摄'), bg: '#8e8e93', fg: '#1c1c1e' },
  { id: 'messages', name: tr('信息'), bundle: 'com.apple.MobileSMS', glyph: tr('信'), bg: '#34c759', fg: '#fff' },
  { id: 'notes', name: tr('备忘录'), bundle: 'com.apple.mobilenotes', glyph: tr('记'), bg: '#ffcc00', fg: '#1c1c1e' },
  { id: 'weather', name: tr('天气'), bundle: 'com.apple.weather', glyph: tr('晴'), bg: '#2f80ed', fg: '#fff' },
  { id: 'maps', name: tr('地图'), bundle: 'com.apple.Maps', glyph: tr('图'), bg: '#7cc576', fg: '#1c1c1e' },
  { id: 'settings', name: tr('设置'), bundle: 'com.apple.Preferences', glyph: tr('设'), bg: '#8e8e93', fg: '#fff' },
  { id: 'appstore', name: 'App Store', bundle: 'com.apple.AppStore', glyph: 'A', bg: '#0a84ff', fg: '#fff' },
  { id: 'health', name: tr('健康'), bundle: 'com.apple.Health', glyph: tr('康'), bg: '#fff', fg: '#ff2d55' },
  { id: 'wallet', name: tr('钱包'), bundle: 'com.apple.Passbook', glyph: tr('钱'), bg: '#1c1c1e', fg: '#fff' }
];
const DOCK = [
  { id: 'phone', name: tr('电话'), bundle: 'com.apple.mobilephone', glyph: tr('话'), bg: '#34c759', fg: '#fff' },
  { id: 'safari', name: 'Safari', bundle: 'com.apple.mobilesafari', glyph: 'S', bg: '#fff', fg: '#0a84ff' },
  { id: 'mail', name: tr('邮件'), bundle: 'com.apple.mobilemail', glyph: tr('邮'), bg: '#2f80ed', fg: '#fff' },
  { id: 'music', name: tr('音乐'), bundle: 'com.apple.Music', glyph: tr('乐'), bg: '#fa2d48', fg: '#fff' }
];
const ALL_APPS = [...APPS, ...DOCK];
const appById = id => ALL_APPS.find(a => a.id === id);
const appByAny = s => ALL_APPS.find(a => a.name === s || a.bundle === s || a.id === s);
const CLOCK_TABS = [['world', tr('世界时钟')], ['alarm', tr('闹钟')], ['stopwatch', tr('秒表')], ['timer', tr('计时器')]];
const ALARMS = [{ time: '07:00', note: tr('工作日'), on: true }, { time: '08:30', note: tr('周末'), on: false }];
const SETTINGS = {
  root: { title: tr('设置'), back: null, groups: [[[tr('无线局域网'), tr('已连接'), 'set:none'], [tr('蓝牙'), tr('打开'), 'set:none']], [[tr('通用'), '', 'set:general'], [tr('显示与亮度'), '', 'set:none'], [tr('电池'), '', 'set:none'], [tr('隐私与安全性'), '', 'set:none']]] },
  general: { title: tr('通用'), back: [tr('设置'), 'set:root'], groups: [[[tr('关于本机'), '', 'set:about'], [tr('软件更新'), '', 'set:none']], [[tr('日期与时间'), '', 'set:none'], [tr('键盘'), '', 'set:none']]] },
  about: { title: tr('关于本机'), back: [tr('通用'), 'set:general'], groups: [[[tr('名称'), 'iPhone', null], [tr('iOS 版本'), '18.1', null], [tr('型号名称'), 'iPhone 15 Pro', null], [tr('容量'), '256 GB', null]]] }
};
const DEMO_DEVICES = [
  { udid: '00008130-001A2B3C4D5E001E', name: 'iPhone 15 Pro', model: 'iPhone16,1', model_name: 'iPhone 15 Pro', family: 'iphone', ios: '18.1', kind: 'iphone', w: 393, h: 852 },
  { udid: '00008112-000C1D2E3F4A5B6C', name: tr('iPad mini（第 6 代）'), model: 'iPad14,1', model_name: tr('iPad mini（第 6 代）'), family: 'ipad', ios: '18.1', kind: 'ipad', w: 1133, h: 744 }  // iPad 默认横屏
];
let demoDev = DEMO_DEVICES[0];
const DW = () => demoDev.w, DH = () => demoDev.h;
function osVersion(device) {
  const family = device.family ?? (/^iPad[0-9]+,[0-9]+$/.test(device.model) ? 'ipad' : /^iPhone[0-9]+,[0-9]+$/.test(device.model) ? 'iphone' : 'unknown');
  const system = family === 'ipad' ? 'iPadOS' : family === 'iphone' ? 'iOS' : '';
  return device.ios ? [system, device.ios].filter(Boolean).join(' ') : '';
}
function deviceOptionLabel(device) {
  const model = device.model_name || device.model;
  if (device.name && model && device.name !== model) return device.name + ' · ' + model;
  return device.name || model || device.udid;
}

const D = {
  screen: 'home', clockTab: 'world', setPage: 'root', generic: '', genericSub: '',
  sw: { running: false, start: 0, acc: 0, laps: [] },
  chats: {
    [tr('妈妈')]: [{ me: false, text: tr('晚上回来吃饭吗？'), t: '18:12' }],
    [tr('王磊')]: [{ me: false, text: tr('明天的评审改到上午十点了'), t: '17:40' }],
    [tr('快递通知')]: [{ me: false, text: tr('您的包裹已到驿站，凭取件码取件'), t: tr('昨天') }]
  },
  chatWho: tr('妈妈'), draft: '', focused: false
};
const swElapsed = () => D.sw.acc + (D.sw.running ? performance.now() - D.sw.start : 0);
function screenName() {
  switch (D.screen) {
    case 'home': return tr('主屏幕');
    case 'lock': return tr('锁定屏幕');
    case 'clock': return `${tr('时钟')} · ${CLOCK_TABS.find(t => t[0] === D.clockTab)[1]}`;
    case 'settings': return D.setPage === 'root' ? tr('设置') : `${tr('设置')} · ${SETTINGS[D.setPage].title}`;
    case 'messages': return tr('信息');
    case 'chat': return `${tr('与「')}${D.chatWho}${tr('」的对话')}`;
    default: return D.generic;
  }
}
function foreground() { return { home: tr('主屏幕'), lock: tr('主屏幕'), clock: tr('时钟'), settings: tr('设置'), messages: tr('信息'), chat: tr('信息') }[D.screen] || D.generic; }

function appTile(a, dock) {
  return `<div class="ph-app" data-act="app:${a.id}" data-label="${esc(a.name)}" data-type="Icon"><div class="ph-icon" style="background:${a.bg};color:${a.fg}">${esc(a.glyph)}</div>${dock ? '' : `<div class="ph-app-name">${esc(a.name)}</div>`}</div>`;
}
const tz = zone => new Date().toLocaleTimeString('zh-CN', { timeZone: zone, hour: '2-digit', minute: '2-digit', hour12: false });
function clockBody() {
  if (D.clockTab === 'stopwatch') {
    const run = D.sw.running;
    const laps = D.sw.laps.map((ms, i) => `<div class="ph-lap" data-label="${tr('计次')} ${i + 1}" data-type="Cell"><span>${tr('计次')} ${i + 1}</span><span>${fmtSw(ms)}</span></div>`).reverse().join('');
    return `<div class="ph-sw"><div class="sw-time" data-label="${tr('秒表时间')}" data-type="StaticText">${fmtSw(swElapsed())}</div>
      <div class="ph-sw-btns"><div class="ph-round gray" data-act="sw:${run ? 'lap' : 'reset'}" data-label="${run ? tr('计次') : tr('复位')}" data-type="Button">${run ? tr('计次') : tr('复位')}</div><div class="ph-round ${run ? 'red' : 'green'}" data-act="sw:toggle" data-label="${run ? tr('停止') : tr('启动')}" data-type="Button">${run ? tr('停止') : tr('启动')}</div></div>
      <div class="ph-laps">${laps}</div></div>`;
  }
  if (D.clockTab === 'world') {
    const rows = [[tr('北京'), 'Asia/Shanghai', tr('今天')], [tr('伦敦'), 'Europe/London', tr('今天，−8 小时')], [tr('纽约'), 'America/New_York', tr('今天，−13 小时')]];
    return `<div class="ph-title" data-label="${tr('世界时钟')}" data-type="StaticText">${tr('世界时钟')}</div>${rows.map(([c, z, sub]) => `<div class="ph-row" data-label="${c}" data-type="Cell"><div class="ph-row-main"><span class="ph-row-sub">${sub}</span><span>${c}</span></div><span class="ph-row-big">${tz(z)}</span></div>`).join('')}`;
  }
  if (D.clockTab === 'alarm') {
    return `<div class="ph-title" data-label="${tr('闹钟')}" data-type="StaticText">${tr('闹钟')}</div>${ALARMS.map(a => `<div class="ph-row${a.on ? '' : ' off'}" data-label="${tr('闹钟')} ${a.time} ${a.note} ${a.on ? tr('开') : tr('关')}" data-type="Cell"><div class="ph-row-main"><span class="ph-row-big">${a.time}</span><span class="ph-row-sub">${tr('闹钟，')}${a.note}</span></div><span class="ph-toggle${a.on ? ' on' : ''}"></span></div>`).join('')}`;
  }
  return `<div class="ph-title" data-label="${tr('计时器')}" data-type="StaticText">${tr('计时器')}</div><div class="sw-time" style="margin-top:80px">05:00</div>
    <div class="ph-sw-btns" style="margin-top:80px"><div class="ph-round gray" data-label="${tr('取消')}" data-type="Button">${tr('取消')}</div><div class="ph-round green" data-act="timer:start" data-label="${tr('启动')}" data-type="Button">${tr('启动')}</div></div>`;
}
function screenHtml() {
  switch (D.screen) {
    case 'home': return `<div class="ph-home"><div class="ph-grid">${APPS.map(a => appTile(a)).join('')}</div><div class="ph-dock">${DOCK.map(a => appTile(a, true)).join('')}</div></div>`;
    case 'lock': { const d = new Date(); return `<div class="ph-lock"><div class="ph-lock-date">${d.toLocaleDateString(LANG === 'en' ? 'en-US' : 'zh-CN', { month: 'long', day: 'numeric', weekday: 'long' })}</div><div class="ph-lock-time">${hhmm(d)}</div><div class="ph-lock-hint" data-label="${tr('向上轻扫以打开')}" data-type="StaticText">${tr('向上轻扫以打开')}</div></div>`; }
    case 'clock': return `<div class="ph-clock"><div class="ph-body">${clockBody()}</div><div class="ph-tabbar">${CLOCK_TABS.map(([k, l]) => `<div class="ph-tab${D.clockTab === k ? ' on' : ''}" data-act="tab:${k}" data-label="${l}" data-type="Button">${l}</div>`).join('')}</div></div>`;
    case 'settings': {
      const pg = SETTINGS[D.setPage];
      return `<div class="ph-set">${pg.back ? `<div class="ph-nav"><span class="ph-back" data-act="${pg.back[1]}" data-label="${pg.back[0]}" data-type="Button">‹ ${pg.back[0]}</span></div>` : ''}<div class="ph-title" data-label="${pg.title}" data-type="StaticText">${pg.title}</div>${pg.groups.map(g => `<div class="ph-group">${g.map(([l, v, act]) => `<div class="ph-cell"${act ? ` data-act="${act}"` : ''} data-label="${l}${v ? ' ' + v : ''}" data-type="Cell"><span>${l}</span><span class="ph-cell-v">${v}${act ? ' ›' : ''}</span></div>`).join('')}</div>`).join('')}</div>`;
    }
    case 'messages': return `<div class="ph-msgs"><div class="ph-title" data-label="${tr('信息')}" data-type="StaticText">${tr('信息')}</div><div class="ph-search" data-label="${tr('搜索')}" data-type="SearchField">${tr('搜索')}</div>${Object.entries(D.chats).map(([who, msgs]) => { const last = msgs[msgs.length - 1]; return `<div class="ph-thread" data-act="open-chat:${esc(who)}" data-label="${esc(who)}" data-type="Cell"><div class="ph-avatar">${esc(who[0])}</div><div class="ph-thread-text"><span class="ph-thread-name">${esc(who)}</span><span class="ph-thread-prev">${esc(last.text)}</span></div><span class="ph-thread-time">${esc(last.t)}</span></div>`; }).join('')}</div>`;
    case 'chat': {
      const msgs = D.chats[D.chatWho] || [], has = D.draft.length > 0;
      return `<div class="ph-chat"><div class="ph-chat-head"><span class="ph-back" data-act="chat:back" data-label="${tr('信息')}" data-type="Button">‹ ${tr('信息')}</span><div class="ph-chat-who"><div class="ph-avatar">${esc(D.chatWho[0])}</div><span>${esc(D.chatWho)}</span></div><span></span></div>
        <div class="ph-chat-body">${msgs.map(m => `<div class="ph-bubble ${m.me ? 'out' : 'in'}" data-label="${esc(m.text)}" data-type="StaticText">${esc(m.text)}</div>`).join('')}</div>
        <div class="ph-chat-input"><div class="ph-field${has ? '' : ' empty'}${D.focused ? ' focus' : ''}" data-act="chat:focus" data-label="${tr('信息')}" data-type="TextField">${has ? esc(D.draft) : tr('iMessage 信息')}</div><div class="ph-send${has ? '' : ' off'}" data-act="chat:send" data-label="${tr('发送')}" data-type="Button">${tr('发送')}</div></div>
        ${D.focused ? tr('<div class="ph-kb" data-type="Keyboard">键盘</div>') : ''}</div>`;
    }
    default: return `<div class="ph-generic"><div class="ph-title" data-label="${esc(D.generic)}" data-type="StaticText">${esc(D.generic)}</div><p class="ph-generic-sub">${esc(D.genericSub || tr('演示模式没有模拟这个 App。按主屏幕键或从底部上滑返回。'))}</p></div>`;
  }
}
function renderDemo() {
  const tone = ['messages', 'chat', 'generic'].includes(D.screen) ? ' ph--light' : D.screen === 'settings' ? ' ph--gray' : D.screen === 'clock' ? ' ph--black' : '';
  demoEl.className = 'ph' + tone + (demoDev.kind === 'ipad' ? ' ph--ipad' : '');
  demoEl.innerHTML = `<div class="ph-status"><span>${hhmm()}</span><span class="ph-sys"><span>5G</span><span class="ph-batt"></span></span></div><div class="ph-main">${screenHtml()}</div><div class="ph-ind"></div>`;
  if (S.mode === 'demo') {
    if (S.dstatus) { S.dstatus.foreground = foreground(); S.dstatus.locked = D.screen === 'lock'; }
    S.elements = []; S.elState = 'stale'; renderChips(); if (S.devTab === 'status') renderStatusPanel(); if (S.devTab === 'elements') renderElements(); }
}
function demoOpen(screen) { D.screen = screen; if (screen !== 'chat') D.focused = false; renderDemo(); }
function demoLaunch(a) {
  if (a.id === 'clock') { demoOpen('clock'); }
  else if (a.id === 'settings') { D.setPage = 'root'; demoOpen('settings'); }
  else if (a.id === 'messages') { demoOpen('messages'); }
  else { D.generic = a.name; D.genericSub = ''; demoOpen('generic'); }
  return `${tr('打开了「')}${a.name}${tr('」')}`;
}
function demoAct(act) {
  const i = act.indexOf(':'), k = act.slice(0, i), v = act.slice(i + 1);
  switch (k) {
    case 'app': return demoLaunch(appById(v));
    case 'tab': D.clockTab = v; renderDemo(); return `${tr('切到「')}${CLOCK_TABS.find(t => t[0] === v)[1]}${tr('」')}`;
    case 'set': if (v === 'none') return tr('演示模式没有模拟这一页'); D.setPage = v; renderDemo(); return `${tr('进入「')}${SETTINGS[v].title}${tr('」')}`;
    case 'sw':
      if (v === 'toggle') {
        if (D.sw.running) { D.sw.acc = swElapsed(); D.sw.running = false; renderDemo(); return tr('秒表已停止'); }
        D.sw.start = performance.now(); D.sw.running = true; renderDemo(); return tr('秒表开始计时');
      }
      if (v === 'lap') { D.sw.laps.push(swElapsed()); renderDemo(); return `${tr('记了第')} ${D.sw.laps.length} ${tr('次计次')}`; }
      D.sw = { running: false, start: 0, acc: 0, laps: [] }; renderDemo(); return tr('秒表已复位');
    case 'open-chat': D.chatWho = v; D.draft = ''; D.focused = false; demoOpen('chat'); return `${tr('打开了与「')}${v}${tr('」的对话')}`;
    case 'chat':
      if (v === 'back') { demoOpen('messages'); return tr('回到「信息」列表'); }
      if (v === 'focus') { D.focused = true; renderDemo(); return tr('输入框获得焦点，弹出了键盘'); }
      return demoSend();
    case 'timer': return tr('演示模式没有模拟计时器');
  }
  return tr('这里没有可点的东西');
}
function demoSend() {
  if (!D.draft) return tr('输入框是空的，没有发出任何内容');
  (D.chats[D.chatWho] ||= []).push({ me: true, text: D.draft, t: hhmm() });
  const sent = D.draft; D.draft = ''; renderDemo();
  return `${tr('已发送给「')}${D.chatWho}${tr('」：')}${sent}`;
}
function demoType(text, submit) {
  if (!(D.screen === 'chat' && D.focused)) return { ok: false, message: tr('没有弹出键盘：先在画面里点一下输入框。') };
  D.draft += text; renderDemo();
  return submit ? { ok: true, message: demoSend() } : { ok: true, message: `${tr('已输入「')}${text}${tr('」')}` };
}
function demoKey(key) {
  if (key === 'home') { const wasLocked = D.screen === 'lock'; demoOpen('home'); return wasLocked ? tr('已自动解锁，回到主屏幕') : tr('回到主屏幕'); }
  if (key === 'lock') { demoOpen('lock'); return tr('已锁屏'); }
  return key === 'volume_up' ? tr('音量加（演示画面没有变化）') : tr('音量减（演示画面没有变化）');
}
setInterval(() => {
  if (S.mode === 'demo' && D.screen === 'clock' && D.clockTab === 'stopwatch' && D.sw.running) {
    const el = $('.sw-time', demoEl); if (el) el.textContent = fmtSw(swElapsed());
  }
}, 50);
function demoElements() {
  const R = demoEl.getBoundingClientRect(), k = R.width / DW() || 0;
  return $$('[data-label]', demoEl).map(e => {
    const r = e.getBoundingClientRect();
    return { type: e.dataset.type || 'StaticText', label: e.dataset.label, x: k ? round1((r.left - R.left) / k) : 0, y: k ? round1((r.top - R.top) / k) : 0, width: k ? round1(r.width / k) : 0, height: k ? round1(r.height / k) : 0 };
  });
}

/* ================================================================
 * 画面几何
 * ================================================================ */
