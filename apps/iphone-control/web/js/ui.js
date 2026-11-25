const sleep = ms => new Promise(res => setTimeout(res, ms));
const round1 = v => Math.round(v * 10) / 10;
const pad = n => String(n).padStart(2, '0');
const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
/* 任务总结：Claude Code、Codex 都爱用 Markdown（粗体、列表）。整段先转义，再只认一小部分写法，任何原始 HTML 都进不来：
 * 粗体（只认 **，免得把 __init__ 这类名字当成粗体）、斜体、行内代码、无序和有序列表、标题（显示成粗体段落）、分隔线、空行分段、单个换行，以及 http(s) 链接 */
function mdInline(s) {
  // s 已经转义过；行内代码里的字原样显示，不再认粗体之类
  return s.split(/(`[^`\n]+`)/).map((part, i) => i % 2 ? `<code>${part.slice(1, -1)}</code>` : part
    .replace(/\*\*([^*\n]+?)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[^*\w])\*(?=\S)([^*\n]+?)\*(?![*\w])/g, '$1<em>$2</em>')
    .replace(/\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>')).join('');
}
function mdLite(text) {
  const out = [];
  let para = [], list = null;
  const flushPara = () => { if (para.length) out.push(`<p>${para.map(mdInline).join('<br>')}</p>`); para = []; };
  const flushList = () => { if (list) out.push(`<${list.tag}>${list.items.map(i => `<li>${mdInline(i)}</li>`).join('')}</${list.tag}>`); list = null; };
  for (const raw of esc(String(text ?? '').replace(/\r\n?/g, '\n').trim()).split('\n')) {
    const line = raw.trim();
    let m;
    if (!line || /^([-*_])(\s*\1){2,}$/.test(line)) { flushPara(); flushList(); continue; }
    if ((m = line.match(/^#{1,6}\s+(.+)$/))) { flushPara(); flushList(); out.push(`<p><strong>${mdInline(m[1])}</strong></p>`); continue; }
    const item = line.match(/^[-*+]\s+(.*)$/) || line.match(/^\d{1,3}[.)]\s+(.*)$/);
    if (item) {
      const tag = /^\d/.test(line) ? 'ol' : 'ul';
      flushPara();
      if (!list || list.tag !== tag) { flushList(); list = { tag, items: [] }; }
      list.items.push(item[1]);
      continue;
    }
    // a list item's next, indented line belongs to it
    if (list && /^\s/.test(raw)) { list.items[list.items.length - 1] += ' ' + line; continue; }
    flushList();
    para.push(line);
  }
  flushPara(); flushList();
  return out.join('');
}
const hhmm = (d = new Date()) => `${pad(d.getHours())}:${pad(d.getMinutes())}`;
const mmss = ms => { const s = Math.max(0, Math.floor(ms / 1000)); return `${pad(Math.floor(s / 60))}:${pad(s % 60)}`; };
const fmtSw = ms => { const cs = Math.floor(ms / 10); return `${pad(Math.floor(cs / 6000))}:${pad(Math.floor(cs / 100) % 60)}.${pad(cs % 100)}`; };

/* ---------- 图标：一套 24 网格描边图标 ---------- */
const ICONS = {
  control: '<rect x="5" y="2.5" width="12" height="19" rx="3"/><path d="m13.5 11 7.7 3.5-3.4 1.3-1.2 3.3z" fill="currentColor" stroke="#252a30" stroke-width="1"/>',
  settings: '<path d="m9.6 3-.5 2.1-1.5.9-2.1-.6-2.2 3.8 1.6 1.5v2.6l-1.6 1.5 2.2 3.8 2.1-.6 1.5.9.5 2.1h4.8l.5-2.1 1.5-.9 2.1.6 2.2-3.8-1.6-1.5v-2.6l1.6-1.5-2.2-3.8-2.1.6-1.5-.9-.5-2.1Z"/><circle cx="12" cy="12" r="3"/>',
  chat: '<path d="M20.5 13.5a3 3 0 0 1-3 3H10l-4.5 4V16a3 3 0 0 1-2-2.8V6.5a3 3 0 0 1 3-3h11a3 3 0 0 1 3 3z"/><path d="M8 8h8m-8 4h5"/>',
  display: '<rect x="3" y="4.5" width="18" height="13" rx="2.4"/><path d="M8 20.5h8m-4-3v3"/>',
  home: '<path d="M3.5 10.5 12 3.5l8.5 7V20a1 1 0 0 1-1 1H15v-6H9v6H4.5a1 1 0 0 1-1-1z"/>',
  lock: '<rect x="5" y="11" width="14" height="10" rx="2"/><path d="M8 11V7.5a4 4 0 0 1 8 0V11"/>',
  voldown: '<path d="M11 5 6.5 9H3v6h3.5L11 19z"/><path d="M15.5 12h5"/>',
  volup: '<path d="M11 5 6.5 9H3v6h3.5L11 19z"/><path d="M15.5 12h5M18 9.5v5"/>',
  enter: '<path d="M19 5v7a3 3 0 0 1-3 3H6"/><path d="m10 11-4 4 4 4"/>',
  send: '<path d="M12 19V5"/><path d="m6 11 6-6 6 6"/>',
  trash: '<path d="M4.5 7h15M10 11v6M14 11v6"/><path d="M6.5 7l.8 11.2a2 2 0 0 0 2 1.8h5.4a2 2 0 0 0 2-1.8L17.5 7"/><path d="M9.5 7V5a1 1 0 0 1 1-1h3a1 1 0 0 1 1 1v2"/>',
  arrow: '<path d="M5 12h14"/><path d="m13 6 6 6-6 6"/>',
  chevron: '<path d="m6 9 6 6 6-6"/>',
  pause: '<path d="M9 5v14M15 5v14"/>',
  play: '<path d="M7.5 5v14l11-7z"/>',
  cursor: '<path d="M5 3.5 18.5 10l-6 1.8-2.3 6.2z"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="1.5"/>',
  check: '<path d="m5 12.5 4.5 4.5L19 7.5"/>',
  x: '<path d="M6 6l12 12M18 6 6 18"/>',
  alert: '<path d="M12 4 2.5 20h19z"/><path d="M12 10v4.5M12 17.2v.3"/>',
  loader: '<path d="M12 3a9 9 0 1 1-9 9"/>',
  phone: '<rect x="7" y="2.5" width="10" height="19" rx="2.5"/><path d="M11 18.5h2"/>',
  tablet: '<rect x="4.5" y="2.5" width="15" height="19" rx="2.5"/><path d="M11 18.5h2"/>',
  refresh: '<path d="M20 12a8 8 0 1 1-2.4-5.7"/><path d="M20 4v5h-5"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v5.5M12 7.8v.2"/>',
  dot: '<circle cx="12" cy="12" r="4"/>',
  eye: '<path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12z"/><circle cx="12" cy="12" r="3"/>',
  tap: '<circle cx="12" cy="12" r="3"/><circle cx="12" cy="12" r="8"/>',
  keyboard: '<rect x="3" y="6" width="18" height="12" rx="2"/><path d="M7 10h.01M11 10h.01M15 10h.01M7.5 14h9"/>',
  grid: '<rect x="4" y="4" width="6.5" height="6.5" rx="1.5"/><rect x="13.5" y="4" width="6.5" height="6.5" rx="1.5"/><rect x="4" y="13.5" width="6.5" height="6.5" rx="1.5"/><rect x="13.5" y="13.5" width="6.5" height="6.5" rx="1.5"/>',
  link: '<path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1"/><path d="M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"/>',
  panel: '<rect x="3.5" y="4.5" width="17" height="15" rx="2"/><path d="M14.5 4.5v15"/>',
  search: '<circle cx="11" cy="11" r="6.5"/><path d="m16 16 4 4"/>',
  zoomin: '<circle cx="11" cy="11" r="6.5"/><path d="m16 16 4 4M8.5 11h5M11 8.5v5"/>',
  zoomout: '<circle cx="11" cy="11" r="6.5"/><path d="m16 16 4 4M8.5 11h5"/>',
  conversation: '<path d="M6 4.5h12a2.5 2.5 0 0 1 2.5 2.5v8a2.5 2.5 0 0 1-2.5 2.5h-7l-5 3v-3A2.5 2.5 0 0 1 3.5 15V7A2.5 2.5 0 0 1 6 4.5Z"/><path d="M8 9h8M8 12.5h5"/>',
  bot: '<rect x="4.5" y="7.5" width="15" height="11" rx="3"/><path d="M12 3.5v4M9.5 12.5v1.5M14.5 12.5v1.5"/>'
};
function icon(name, extra = '') {
  return `<svg class="od-icon ${extra}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name] || ''}</svg>`;
}
function hydrateIcons(root = document) { $$('[data-icon]', root).forEach(el => { el.innerHTML = icon(el.dataset.icon); }); }

// All five controls share the same popover. Each select supplies its current
// options, value and disabled state and still emits the usual change event.
function positionPopover(button, popup, preferAbove = false) {
  if (!popup.matches(':popover-open')) return;
  const rect = button.getBoundingClientRect();
  const above = rect.top - 14, below = innerHeight - rect.bottom - 14;
  const height = popup.scrollHeight + popup.offsetHeight - popup.clientHeight;
  const openAbove = preferAbove ? above >= height || above >= below : below < height && above > below;
  popup.style.maxHeight = Math.max(0, openAbove ? above : below) + 'px';
  popup.style.top = (openAbove ? rect.top - 6 - popup.offsetHeight : rect.bottom + 6) + 'px';
  popup.style.left = Math.max(8, Math.min(rect.right - popup.offsetWidth, innerWidth - popup.offsetWidth - 8)) + 'px';
}
function bindPicker(select) {
  const id = select.dataset.picker;
  select.insertAdjacentHTML('afterend', `
    <button type="button" class="picker-trigger" id="${id}" popovertarget="${id}-options" aria-haspopup="listbox" aria-controls="${id}-options" aria-expanded="false" aria-labelledby="${id}-label ${id}-name">
      <span class="od-truncate" id="${id}-name"></span><span class="ic">${icon('chevron')}</span>
    </button>
    <div id="${id}-options" class="picker-menu glass" role="listbox" aria-labelledby="${id}-label" popover="auto" tabindex="-1" autofocus></div>`);
  const button = document.getElementById(id), menu = document.getElementById(id + '-options');
  const name = document.getElementById(id + '-name');
  const position = () => positionPopover(button, menu, select.hasAttribute('data-open-above'));
  const active = () => menu.querySelector('[data-active]');
  let optionRows = '', prefix = '', lastKey = 0;

  function highlight(row) {
    if (!row || row.hasAttribute('data-active') || row.getAttribute('aria-disabled') === 'true') return;
    active()?.removeAttribute('data-active');
    row.setAttribute('data-active', '');
    menu.setAttribute('aria-activedescendant', row.id);
  }
  function renderOptions(opening) {
    const rows = Array.from(select.options, (o, i) =>
      `<div class="picker-option" role="option" id="${id}-opt-${i}" data-value="${esc(o.value)}" aria-selected="${o.selected}" aria-disabled="${o.disabled}"><span${o.lang ? ` lang="${esc(o.lang)}"` : ''}>${esc(o.label)}</span>${icon('check')}</div>`
    ).join('');
    if (!opening && rows === optionRows) return;
    // A device poll may reorder, rename or remove rows while keyboard navigation is in progress.
    const previous = opening ? null : active()?.dataset.value;
    optionRows = rows;
    menu.innerHTML = rows;
    menu.removeAttribute('aria-activedescendant');
    const enabled = $$('[aria-disabled="false"]', menu);
    highlight(enabled.find(row => row.dataset.value === previous) || enabled.find(row => row.getAttribute('aria-selected') === 'true') || enabled[0]);
    requestAnimationFrame(() => { position(); active()?.scrollIntoView({ block: 'nearest' }); });
  }
  function sync() {
    const label = select.selectedOptions[0]?.label || '';
    if (label !== name.textContent) name.textContent = label;
    button.disabled = select.disabled;
    button.title = select.title;
    if (menu.matches(':popover-open')) {
      if (button.disabled || !button.getClientRects().length) menu.hidePopover();
      else renderOptions(false);
    }
  }
  function choose(row) {
    if (!row || button.disabled || row.getAttribute('aria-disabled') === 'true') return;
    const changed = select.value !== row.dataset.value;
    select.value = row.dataset.value;
    menu.hidePopover();
    button.focus();
    sync();
    if (changed) select.dispatchEvent(new Event('change', { bubbles: true }));
  }
  menu.addEventListener('beforetoggle', ev => {
    const opening = ev.newState === 'open';
    button.setAttribute('aria-expanded', String(opening));
    if (opening) { prefix = ''; renderOptions(true); }
  });
  menu.addEventListener('pointermove', ev => highlight(ev.target.closest('[role="option"]')));
  menu.addEventListener('click', ev => choose(ev.target.closest('[role="option"]')));
  button.addEventListener('keydown', ev => {
    if (ev.key === 'ArrowDown' || ev.key === 'ArrowUp') { ev.preventDefault(); menu.showPopover(); }
  });
  menu.addEventListener('keydown', ev => {
    const enabled = $$('[role="option"][aria-disabled="false"]', menu);
    const index = enabled.indexOf(active());
    let next;
    switch (ev.key) {
      case 'ArrowDown': next = enabled[Math.min(index + 1, enabled.length - 1)]; break;
      case 'ArrowUp': next = enabled[Math.max(0, index - 1)]; break;
      case 'Home': next = enabled[0]; break;
      case 'End': next = enabled.at(-1); break;
      case 'Enter': case ' ': ev.preventDefault(); choose(active()); return;
      case 'Tab': menu.hidePopover(); button.focus(); return;
      case 'Escape': ev.preventDefault(); ev.stopPropagation(); menu.hidePopover(); button.focus(); return;
      default:
        if (ev.key.length !== 1 || ev.ctrlKey || ev.metaKey || ev.altKey || ev.isComposing) return;
        prefix = (ev.timeStamp - lastKey < 600 ? prefix : '') + ev.key.toLowerCase();
        lastKey = ev.timeStamp;
        next = enabled.find(row => row.textContent.toLowerCase().startsWith(prefix));
    }
    ev.preventDefault();
    highlight(next);
    next?.scrollIntoView({ block: 'nearest' });
  });
  new ResizeObserver(position).observe(button.closest('dialog, .pane') || button);
  window.addEventListener('resize', position);
  sync();
  return sync;
}
const pickerSync = Object.fromEntries($$('select[data-picker]').map(select => [select.dataset.picker, bindPicker(select)]));

