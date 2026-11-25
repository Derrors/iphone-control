const scopeMenu = $('#scope-menu'), scopeBtn = $('#scope-btn');
function scopeName(bundle) {
  const a = (S.scopeApps || []).find(x => x.bundle_id === bundle);
  return a ? a.name : appLabel(bundle);
}
function scopeText(apps) { return apps.map(scopeName).join(tr('、')); }
/* 按钮在输入框左边：面板的左边对齐按钮（共用的 positionPopover 是右对齐） */
function placeScopeMenu() {
  positionPopover(scopeBtn, scopeMenu, true);
  if (!scopeMenu.matches(':popover-open')) return;
  const r = scopeBtn.getBoundingClientRect();
  scopeMenu.style.left = Math.max(8, Math.min(r.left, innerWidth - scopeMenu.offsetWidth - 8)) + 'px';
}
function renderScopePick() {
  $('#scope-pick').hidden = S.mode !== 'live';
  const n = S.scope.length;
  $('#scope-name').textContent = n ? scopeText(S.scope) : tr('所有 App');
  scopeBtn.toggleAttribute('data-scoped', n > 0);
  scopeBtn.disabled = !!activeTask() || !S.udid;
  scopeBtn.title = n ? `${tr('任务只在这些 App 里操作：')}${scopeText(S.scope)}` : tr('任务可以操作所有 App；点这里限定范围');
  if (scopeBtn.disabled && scopeMenu.matches(':popover-open')) scopeMenu.hidePopover();
}
async function loadScopeApps() {
  if (S.scopeApps && S.scopeAppsFor === S.udid) return;
  const target = S.udid;
  S.scopeErr = ''; S.scopeApps = null; renderScopeList();
  try {
    const apps = ((await api('/api/apps?system=1')).apps || []).slice().sort((a, b) => a.name.localeCompare(b.name));
    if (target !== S.udid) return;
    S.scopeApps = apps; S.scopeAppsFor = target;
  } catch (e) { S.scopeErr = String(e.message || e); }
  renderScopeList(); renderScopePick(); placeScopeMenu();
}
function renderScopeList() {
  const list = $('#scope-list'), q = $('#scope-q').value.trim().toLowerCase();
  const picked = new Set(S.scope);
  $('#scope-count').textContent = picked.size ? `${tr('已选')} ${picked.size}` : tr('不限制');
  if (S.scopeErr) { list.innerHTML = `<p class="empty-note">${esc(tr('读不到 App 列表：'))}${esc(S.scopeErr)}</p>`; return; }
  if (!S.scopeApps) { list.innerHTML = `<p class="empty-note">${esc(tr('正在读取 App…'))}</p>`; return; }
  // 选中的排在前面（包括列表里没有的，例如换了设备后），其余按名字
  const known = new Set(S.scopeApps.map(a => a.bundle_id));
  const rows = [...S.scope.filter(b => !known.has(b)).map(b => ({ name: scopeName(b), bundle_id: b })), ...S.scopeApps]
    .filter(a => !q || a.name.toLowerCase().includes(q) || a.bundle_id.toLowerCase().includes(q))
    .sort((a, b) => Number(picked.has(b.bundle_id)) - Number(picked.has(a.bundle_id)));
  list.innerHTML = rows.length ? rows.map(a => `<label class="scope-row"><input type="checkbox" value="${esc(a.bundle_id)}"${picked.has(a.bundle_id) ? ' checked' : ''}><span class="scope-app"><b>${esc(a.name)}</b><small>${esc(a.bundle_id)}</small></span></label>`).join('')
    : `<p class="empty-note">${esc(tr('没有名称或 bundle id 含「'))}${esc(q)}${esc(tr('」的 App。'))}</p>`;
}
function toggleScope(bundle, on) {
  S.scope = on ? [...new Set([...S.scope, bundle])] : S.scope.filter(b => b !== bundle);
  $('#scope-count').textContent = S.scope.length ? `${tr('已选')} ${S.scope.length}` : tr('不限制');
  renderScopePick();
}
scopeMenu.addEventListener('beforetoggle', ev => {
  const opening = ev.newState === 'open';
  scopeBtn.setAttribute('aria-expanded', String(opening));
  if (opening) { $('#scope-q').value = ''; renderScopeList(); loadScopeApps(); requestAnimationFrame(placeScopeMenu); }
  else renderScopeList();
});
scopeMenu.addEventListener('toggle', ev => { if (ev.newState === 'open') { placeScopeMenu(); $('#scope-q').focus(); } });
addEventListener('resize', placeScopeMenu);
$('#scope-list').addEventListener('change', ev => { if (ev.target.matches('input[type="checkbox"]')) toggleScope(ev.target.value, ev.target.checked); });
$('#scope-q').addEventListener('input', renderScopeList);
$('#scope-clear').addEventListener('click', () => { S.scope = []; renderScopeList(); renderScopePick(); });
$('#scope-done').addEventListener('click', () => { scopeMenu.hidePopover(); scopeBtn.focus(); });
