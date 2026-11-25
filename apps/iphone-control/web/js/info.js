function applyInfo(v) {
  const udid = v.udid || null;
  S.login = v.login || null; S.readOnly = S.login?.role === 'view';
  document.documentElement.toggleAttribute('data-readonly', S.readOnly);
  if (S.readOnly) $('#readonly-note').textContent = `${tr('只读登录')}（${S.login.name}）${tr('：能看画面、设备状态和任务，不能操作手机，也不能开始、暂停或确认任务。')}`;
  S.video = v.video || null; S.deviceVideo = !!v.device_video;
  S.size = v.width > 0 && v.height > 0 ? { w: v.width, h: v.height } : null;
  frame.toggleAttribute('data-absent', v.state === 'Absent' && !udid);
  const changed = S.udid !== udid, display = v.display || null;
  if (changed || S.display?.id !== display?.id) {
    cancelGestures();  // queue a release for the original device and view
    T.fails = 0;
    S.rtt = null; S.elements = []; S.elState = 'idle';
    highlight(null); marks.replaceChildren(); hoverDot = null;
    if (changed) {
      S.daemon = null; S.dstatus = null; S.apps = []; S.appsState = 'idle'; appNames.map = null; appNames.loading = false;
      T.warm = 0; V.noDevice = false; V.devRetries = 0; clearTimeout(V.devTimer); V.devTimer = 0;
    }
  }
  const reconnect = feedId && (changed || S.display?.id !== display?.id);
  S.udid = udid; S.display = display;
  if (changed) { renderManual(); syncSend(); }
  S.family = v.family || 'unknown'; frame.dataset.kind = S.family; S.deviceName = v.device || '';
  S.liveTouch = T.fails < 2 && !!v.live_touch;
  if (reconnect) {
    connect();
    loadDeviceState();
    if (changed && S.devTab === 'apps') loadApps();
  } else if (v.feed_open === false && S.stream === 'ok') {
    feedError();
  }
  if (v.warning && !S.infoWarning) show(v.warning, 'err');
  S.infoWarning = v.warning || '';
}
function signButton(label) {
  return `<button type="button" data-action="sign"${S.signing ? ' disabled' : ''} class="btn btn-primary btn-sm">${esc(S.signing ? tr('正在签名安装…') : label)}</button>`;
}
/* 没有画面时，按守护进程的状态说清楚卡在哪、怎么办 */
function stageNote() {
  const d = S.daemon, name = d?.device?.name || S.deviceName || tr('设备');
  switch (d?.state || (frame.hasAttribute('data-absent') ? 'Absent' : null)) {
    case 'Absent': return '<span class="idle-heading">' + tr('未连接设备') + '</span>' + tr('用数据线连接 iPhone 或 iPad。');
    case 'NeedsTrust': case 'DeveloperModeOff':
      return `${esc(name)} ${esc(tr('还没设置好（配对、开发者模式）。在终端运行'))} <code>iphone-control setup --reconfigure</code>${esc(tr('，选「手机」。'))}`;
    case 'NeedsSigning':
      return d.signed_in
        ? `${esc(name)} ${esc(tr('上还没装 WebDriverAgent。'))}<br>${signButton(tr('签名并安装 WDA'))}`
        : `${esc(name)} ${esc(tr('上还没装 WebDriverAgent，需要先登录 Apple ID：在终端运行'))} <code>iphone-control setup --reconfigure</code>${esc(tr('，选「Apple ID」。'))}`;
    case 'WdaStarting':
      // 启动失败过就把原因摆出来（守护进程会自动重试），不要一直说「通常 10–60 秒」
      // 启动失败常见的一种是签名证书变了（给另一台设备重签时换了证书），重签这台就好；登录了 Apple ID 就给个按钮
      if (d.last_error) return `${esc(name)}${esc(tr('：WebDriverAgent 启动失败，会自动重试'))}<br><span class="stage-err">${esc(d.last_error)}</span>${d.signed_in ? `<br>${signButton(tr('重新签名并安装 WDA'))}` : ''}`;
      return `${esc(tr('正在'))} ${esc(name)} ${esc(tr('上启动 WebDriverAgent，通常 10–60 秒…'))}`;
    case 'Preparing':
      // 原生控制（iOS 27 起不用 WDA）：只要挂开发者镜像、连无障碍服务
      if (d.last_error) return `${esc(name)}${esc(tr('：准备设备失败，会自动重试'))}<br><span class="stage-err">${esc(d.last_error)}</span>`;
      return esc(tr('正在准备设备（挂载开发者镜像、连接无障碍服务）…'));
    case 'NeedsUnlock': return esc(tr('设备重启后还没解锁过，在设备上手动解锁一次，画面就会出来。'));
    case 'NeedsAutomationPin': return esc(tr('设备重启后首次启用 UI 自动化，请在设备上输入锁屏密码。'));
    default: return esc(tr('没有收到设备画面，3 秒后自动重试。'));
  }
}
function showStageNote() {
  if (S.mode !== 'live' || S.stream !== 'error') return;
  const html = stageNote(); if (stageMsg.dataset.note !== html) { stageMsg.innerHTML = html; stageMsg.dataset.note = html; }  // 不变就不重画，免得按钮丢了焦点
  stageMsg.hidden = false;
}
// Opening the popover reads the host's cached report, even when no device is connected.
const updateDialog = $('#updates-dialog'), updateCheck = $('#update-check');
const aboutUpdates = $('#about-updates'), updateCommand = $('#update-command'), copyStatus = $('#update-copy-status');
let updatePoll, readingUpdates = false;
const updateText = (zh, en) => LANG === 'en' ? en : zh;
function newerRelease(r) {
  return r.available_version && ['available', 'brew_check', 'platform_pending', 'manual_only', 'unsupported'].includes(r.availability);
}
function drawUpdateBadge(r) {
  const newer = newerRelease(r);
  $('#update-dot').hidden = !newer;
  const label = tr('关于与更新') + (newer ? updateText(' · 新版本 ', ' · New release ') + r.available_version : '');
  aboutUpdates.title = label;
  aboutUpdates.setAttribute('aria-label', label);
}
function setUpdateCommand(command) {
  if (updateCommand.textContent !== command) {
    copyStatus.textContent = '';
    updateCommand.textContent = command;
  }
  $('#update-command-block').hidden = $('#update-copy').hidden = !command;
}
// The first changelog item is an excerpt. The release page has the full notes.
function updateExcerpt(markdown) {
  const line = markdown.split(/\r?\n/).map(s => s.trim()).find(s => s && !/^#|^---|^<!--/.test(s)) || '';
  return line.replace(/^(?:[-*+] |\d+[.)] )/, '').replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1').replace(/\*\*|__/g, '').replace(/`([^`]+)`/g, '$1');
}
function drawUpdates(r) {
  const t = updateText, unknown = t('未知', 'Unknown');
  const channels = { managed: t('受管脚本安装', 'Managed installer'), script: t('手动脚本安装', 'Manual installer'),
    development: t('源码构建', 'Source build'), homebrew: 'Homebrew', debian: 'Debian', unknown };
  const states = { development: t('开发构建 · 未绑定发布仓库', 'Development build · No release repository'),
    unchecked: t('尚未检查', 'Not checked yet'), current: t('已是最新版', 'Up to date'),
    ahead: t('当前版本领先于公开稳定版', 'Ahead of the public stable release'),
    available: t('有可用更新', 'Update available'), platform_pending: t('已发布，当前平台安装包准备中', 'Released; waiting for this platform’s package'),
    brew_check: t('请在主机确认 Homebrew 可升级版本', 'Check Homebrew on the host'),
    unsupported: t('当前平台没有发行包', 'No release package for this platform'),
    manual_only: t('此版本需要手动升级', 'Manual upgrade required') };
  const policies = { manual: t('仅手动检查', 'Manual checks'), notify: t('后台检查，不自动安装', 'Check only'),
    'auto-patch': t('空闲时自动安装补丁版', 'Install patches when idle') };
  const newer = newerRelease(r);
  $('#update-version').textContent = newer ? (r.availability === 'available' ? t('可更新至 ', 'Update to ') : t('新版本 ', 'New release ')) + r.available_version : 'Viewer ' + (r.running_version || unknown);
  $('#update-notes').textContent = newer ? updateExcerpt(r.release_notes || '') : '';
  const facts = $('#update-facts'); facts.replaceChildren();
  const row = (label, value) => { const dt = document.createElement('dt'), dd = document.createElement('dd');
    dt.textContent = label; dd.textContent = value; facts.append(dt, dd); };
  row(t('当前 Viewer', 'Running Viewer'), r.running_version || unknown);
  row(t('发布者', 'Publisher'), r.publisher || t('未绑定', 'Unbound'));
  row(t('安装方式', 'Installation'), (channels[r.install_method] || unknown) + (r.pinned ? t(' · 固定版本', ' · pinned') : ''));
  row(t('磁盘 / 守护进程', 'Disk / daemon'), (r.installed_version || unknown) + ' / ' + (r.daemon_version || unknown));
  row(t('更新策略', 'Update policy'), policies[r.policy] || unknown);
  row(t('上次检查成功', 'Last successful check'), r.last_success_at ? new Date(r.last_success_at * 1000).toLocaleString(LANG) : '—');
  if (r.available_version) row(t('公开稳定版', 'Stable release'), r.available_version + (r.published_at ? ' · ' + r.published_at.slice(0,10) : ''));
  const errors = { network: t('无法连接发布仓库', 'Release repository unreachable'), invalid_release: t('发行元数据无效', 'Invalid release metadata'),
    rate_limited: t('请求受限', 'Rate limited'), http_error: t('发布仓库返回错误', 'Release repository returned an error') };
  const failure = errors[r.error_code] || r.error_code || r.check_error;
  const result = failure ? t('检查失败：', 'Check failed: ') + failure : states[r.availability] || unknown;
  if (r.retry_after && r.retry_after * 1000 > Date.now()) row(t('可重试时间', 'Retry after'), new Date(r.retry_after * 1000).toLocaleString(LANG));
  const status = $('#update-result');
  status.textContent = r.checking ? tr('检查中…') : newer && !failure && r.availability === 'available' ? '' : result;
  status.dataset.error = String(Boolean(failure && !r.checking));
  $('#update-guide').textContent = r.guidance || '';
  setUpdateCommand(failure ? '' : r.upgrade_command || '');
  $('#update-copy').title = r.guidance || tr('复制升级命令');
  $('#update-policy').textContent = r.install_method === 'managed' && !r.pinned ?
    t('本机策略：', 'Local policy: ') + 'iphone-control update policy ' + (r.policy === 'auto-patch' ? 'manual' : 'auto-patch') : '';
  const outcomes = { installed: t('安装成功', 'Installed'), restored: t('升级失败，已恢复旧版本', 'Previous version restored'),
    failed: t('更新失败', 'Update failed'),
    needs_operator: t('等待手动升级', 'Waiting for manual upgrade'),
    busy: t('等待空闲', 'Waiting for idle'), recovery_failed: t('需要手动恢复', 'Recovery needs attention') };
  const last = r.last_update?.status;
  const outcome = last === 'needs_operator' ? t('需手动运行 iphone-control update', 'Run iphone-control update manually') : outcomes[last] || last;
  $('#update-outcome').textContent = r.last_update ? [r.last_update.target_version, outcome, r.last_update.detail].filter(Boolean).join(' · ') : '';
  $('#update-mismatch').textContent = [r.installed_version, r.daemon_version].some(v => v && v !== r.running_version) ?
    t('进程版本不同。重新打开 Viewer，重连 MCP 客户端，并按安装渠道重启 daemon。', 'Versions differ. Reopen Viewer, reconnect MCP clients and restart the daemon through its installation channel.') : '';
  const more = $('#update-more');
  const attention = last && last !== 'installed' ? outcomes[last] || last :
    $('#update-mismatch').textContent ? t('版本不一致', 'Version mismatch') : '';
  more.dataset.attention = String(Boolean(attention));
  more.firstElementChild.textContent = attention || tr('版本详情');
  const link = $('#update-release');
  link.hidden = !r.release_url?.startsWith('https://');
  if (link.hidden) link.removeAttribute('href'); else link.href = r.release_url;
  updateCheck.hidden = !r.publisher;
  updateCheck.disabled = r.checking || !r.publisher;
  updateCheck.title = tr(r.checking ? '检查中…' : '检查更新');
  updateCheck.setAttribute('aria-label', updateCheck.title);
  updateCheck.firstElementChild.classList.toggle('spin', r.checking);
}
async function loadUpdates(check = false) {
  if (readingUpdates) return;
  readingUpdates = true;
  clearTimeout(updatePoll);
  updateCheck.disabled = true;
  try {
    if (check) { $('#update-result').textContent = tr('检查中…'); await api('/api/updates/check', {}); }
    const report = await api('/api/updates');
    drawUpdateBadge(report);
    drawUpdates(report);
    if (report.checking && updateDialog.matches(':popover-open')) updatePoll = setTimeout(loadUpdates, 700);
  } catch (e) {
    setUpdateCommand('');
    $('#update-result').textContent = e.message;
    $('#update-result').dataset.error = 'true';
    updateCheck.disabled = updateCheck.hidden;
    updateCheck.firstElementChild.classList.remove('spin');
    updateCheck.title = tr('检查更新');
    updateCheck.setAttribute('aria-label', updateCheck.title);
  } finally {
    readingUpdates = false;
  }
}
function positionUpdates() {
  if (updateDialog.matches(':popover-open')) positionPopover(aboutUpdates, updateDialog, true);
}
function showUpdateDetails(show) {
  $('#update-details').hidden = !show;
  $('#update-more').setAttribute('aria-expanded', String(show));
  positionUpdates();
}
updateDialog.addEventListener('beforetoggle', ev => {
  const opening = ev.newState === 'open';
  aboutUpdates.setAttribute('aria-expanded', String(opening));
  if (!opening) { clearTimeout(updatePoll); return; }
  copyStatus.textContent = '';
  showUpdateDetails(false);
  updateDialog.scrollTop = 0;
  requestAnimationFrame(positionUpdates);
  loadUpdates();
});
$('#update-more').addEventListener('click', () => showUpdateDetails($('#update-details').hidden));
new ResizeObserver(positionUpdates).observe(updateDialog);
new ResizeObserver(positionUpdates).observe($('#pane-agent'));
window.addEventListener('resize', positionUpdates);
$('#update-copy').addEventListener('click', async () => {
  const command = updateCommand.textContent;
  if (!command) return;
  try {
    await navigator.clipboard.writeText(command);
    copyStatus.textContent = tr('已复制');
  } catch {
    const range = document.createRange(), selection = window.getSelection();
    showUpdateDetails(true);
    range.selectNodeContents(updateCommand);
    selection.removeAllRanges();
    selection.addRange(range);
    let copied = false;
    try { copied = document.execCommand('copy'); } catch { /* The command stays selected for manual copy. */ }
    copyStatus.textContent = tr(copied ? '已复制' : '请选中命令并手动复制。');
  }
});
updateCheck.addEventListener('click', () => loadUpdates(true));
// Refresh the host's cached report without initiating a release check.
const refreshVisibleUpdates = () => { if (!document.hidden) loadUpdates(); };
refreshVisibleUpdates();
setInterval(refreshVisibleUpdates, 60000);
document.addEventListener('visibilitychange', refreshVisibleUpdates);

let tasksAskedAt = 0;
async function loadInfo() {
  const seq = ++infoSequence, epoch = feedEpoch;
  try {
    // at most every 3 s as when it had a poll of its own (the list is about 10 KB with 50 tasks)
    const tasks = wantsTasks() && Date.now() - tasksAskedAt >= 3000;
    if (tasks) tasksAskedAt = Date.now();
    const r = await fetch('/api/info?feed=' + encodeURIComponent(feedId) + (tasks ? '&tasks=1' : ''), { headers: { 'X-Ictl-Lang': LANG }, signal: AbortSignal.timeout(4800) });
    if (loggedOut(r) || !r.ok) throw new Error('offline');
    const v = await r.json();
    if (seq === infoSequence && epoch === feedEpoch) applyInfo(v);
    if (tasks && Array.isArray(v.tasks)) adoptTask(v.tasks);
  } catch {
    if (seq === infoSequence && epoch === feedEpoch) feedError();
  }
}
/* 标签页在后台时不问（看不见的页面不用跟），回到前台马上补一次（见 layout.js 的 visibilitychange） */
async function pollInfo() {
  if (!document.hidden) await loadInfo();
  setTimeout(pollInfo, 2000);
}
