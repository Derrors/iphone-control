const TASK_STATUS = {
  idle: { label: tr('空闲'), tone: 'muted', icon: 'dot' },
  running: { label: tr('执行中'), tone: 'accent', icon: 'loader' },
  paused: { label: tr('已暂停'), tone: 'warn', icon: 'pause' },
  takeover: { label: tr('你在接管'), tone: 'user', icon: 'cursor' },
  waiting: { label: tr('等你确认'), tone: 'warn', icon: 'alert' },
  success: { label: tr('已完成'), tone: 'ok', icon: 'check' },
  failed: { label: tr('没有完成'), tone: 'danger', icon: 'x' },
  stopped: { label: tr('已停止'), tone: 'muted', icon: 'stop' }
};
const STEP_STATUS = {
  running: { label: tr('执行中'), tone: 'accent', icon: 'loader' },
  ok: { label: tr('完成'), tone: 'ok', icon: 'check' },
  error: { label: tr('失败'), tone: 'danger', icon: 'x' },
  waiting: { label: tr('等你确认'), tone: 'warn', icon: 'alert' },
  rejected: { label: tr('已拒绝'), tone: 'muted', icon: 'x' },
  stopped: { label: tr('已中止'), tone: 'muted', icon: 'stop' }
};
const ACTIVE = ['running', 'paused', 'takeover', 'waiting'];
const badge = st => `<span class="badge tone-${st.tone}">${icon(st.icon, st.icon === 'loader' ? 'spin' : '')}<span>${esc(st.label)}</span></span>`;

/* ---------- 元素与状态 ---------- */
const shell = $('#shell'), stage = $('#stage'), frame = $('#frame'), demoEl = $('#demo'), marks = $('#marks'), stageMsg = $('#stage-msg');
const statusEl = $('#status'), promptEl = $('#prompt'), promptErr = $('#prompt-error'), typeEl = $('#type-text');
const turnsEl = $('#turns'), stopDialog = $('#stop-dialog'), deviceEl = $('#device-value');
/* 实时画面：H.264 画在 canvas 上（见「H.264 画面」），否则是 MJPEG 的 img */
const cv = $('#live-v'), cvx = cv.getContext('2d');
const V = { on: false, off: false, noDevice: false, device: false, crop: false, rot: null, frameRotation: null, codecName: '', ctrl: null, dec: null, got: false, label: '', ts: 0, bytes: 0, rate: null, stream: '', recv: 0, acked: 0, acking: 0, reported: 0 };
/* MJPEG 画面横屏时也画在 canvas 上转正（见 mjpegTurn） */
const M = { on: false, raf: 0 };
const onCanvas = () => V.on || M.on;
const picEl = () => onCanvas() ? cv : live;
const picW = () => onCanvas() ? cv.width : live.naturalWidth;
const picH = () => onCanvas() ? cv.height : live.naturalHeight;
/* WDA 的画面（本机编码的 H.264、MJPEG）不在手机上转正：WDA 逐帧旋转、重新编码会让 CPU 超出 iOS 的上限，WDA 被杀。
 * iPhone 横屏时帧还是竖的、内容侧着，这里按界面方向转正（同手机自己的视频）；iPad 的帧横屏时本来就是横的，不转。
 * 转屏后视图 id 会变，页面重连画面，所以按出帧时的方向判断就够了 */
function wdaTurn(w, h) {
  const r = S.display?.rotation;
  return (r === 90 || r === 270) && w < h ? r : 0;
}

let live = $('#live');
let feedId = '', feedEpoch = 0, retryTimer = 0, infoSequence = 0;
let frameWidth = 0, frameHeight = 0;
const S = {
  mode: 'detecting',            // detecting | live | demo
  display: null, size: null, video: null, deviceVideo: false, family: 'unknown', stream: 'connecting', rtt: null,
  view: 'agent', devOpen: false, devTab: 'status',
  daemon: null, dstatus: null, statusErr: '',
  devices: [], devicesErr: '',
  apps: [], appsState: 'idle', appsErr: '',
  scope: [], scopeApps: null, scopeAppsFor: '', scopeErr: '',
  elements: [], elState: 'idle', elErr: '',
  tasks: [], expanded: new Set(), seq: 0,
  login: null, readOnly: false,  // 登录用的令牌 {name, role}；只读令牌只能看
  udid: null, deviceName: '', switching: false, signing: false,  // 本页显示的设备：画面、操作、面板都跟着它
  agents: [], agent: null          // 本机可用的 AI（claude / codex）和选中的那个
};
const activeTask = () => S.tasks.find(t => ACTIVE.includes(t.status)) || null;
const guarded = () => { const t = activeTask(); return !!t && t.status !== 'takeover'; };

let statusTimer = 0;
function show(text, kind) {
  statusEl.textContent = text;
  statusEl.classList.toggle('err', kind === 'err');
  statusEl.classList.toggle('warn', kind === 'warn');
  clearTimeout(statusTimer);
  statusEl.classList.add('show');
  statusTimer = setTimeout(() => statusEl.classList.remove('show'), kind ? 6000 : 3500);
}

/* ================================================================
 * 演示画面：模拟的 iPhone 界面
 * ================================================================ */
