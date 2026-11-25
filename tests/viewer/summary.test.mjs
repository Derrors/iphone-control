import assert from 'node:assert/strict';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';
import { viewerPage } from './page.mjs';

const html = viewerPage();

// The page's summary renderer on its own (same approach as actions.test.mjs)
const code = [/^const esc = .*$/m, /^function mdInline\([^]*?^}/m, /^function mdLite\([^]*?^}/m].map(re => {
  const m = html.match(re);
  assert.ok(m, String(re));
  return m[0];
});
const { mdLite } = runInNewContext(code.join('\n') + '\n({ mdLite })', {});

test('what agents write: bold, lists, line breaks', () => {
  assert.equal(mdLite('系统版本是 **iOS 27.0.1**。'), '<p>系统版本是 <strong>iOS 27.0.1</strong>。</p>');
  assert.equal(
    mdLite('五个 App 都打开了：\n\n- **设置**：停在「关于本机」\n- **照片**：显示「精选集」'),
    '<p>五个 App 都打开了：</p><ul><li><strong>设置</strong>：停在「关于本机」</li><li><strong>照片</strong>：显示「精选集」</li></ul>'
  );
  assert.equal(mdLite('1. 打开设置\n2. 点通用'), '<ol><li>打开设置</li><li>点通用</li></ol>');
  assert.equal(mdLite('第一行\n第二行'), '<p>第一行<br>第二行</p>');
  assert.equal(mdLite('## 结果\n完成'), '<p><strong>结果</strong></p><p>完成</p>');
  assert.equal(mdLite('上面\n---\n下面'), '<p>上面</p><p>下面</p>');
  assert.equal(mdLite('- 一项\n  接着这一项\n- 二项'), '<ul><li>一项 接着这一项</li><li>二项</li></ul>');
});

test('code, italics and links, without mangling ordinary text', () => {
  assert.equal(mdLite('运行 `iphone-control **status**`'), '<p>运行 <code>iphone-control **status**</code></p>');
  assert.equal(mdLite('这是 *重点*，2*3=6 不变'), '<p>这是 <em>重点</em>，2*3=6 不变</p>');
  assert.equal(mdLite('com.apple.mobile_cal 和 snake_case_name'), '<p>com.apple.mobile_cal 和 snake_case_name</p>');
  assert.equal(mdLite('Python 的 __init__ 方法'), '<p>Python 的 __init__ 方法</p>');
  assert.equal(
    mdLite('[文档](https://example.com/a?b=1&c=2)'),
    '<p><a href="https://example.com/a?b=1&amp;c=2" target="_blank" rel="noopener noreferrer">文档</a></p>'
  );
});

test('nothing gets through as HTML', () => {
  assert.equal(mdLite('<img src=x onerror=alert(1)>'), '<p>&lt;img src=x onerror=alert(1)&gt;</p>');
  assert.equal(mdLite('[x](javascript:alert(1))'), '<p>[x](javascript:alert(1))</p>');
  assert.equal(mdLite('[x](https://a" onmouseover="b)'), '<p>[x](https://a&quot; onmouseover=&quot;b)</p>');
  assert.equal(mdLite('**<b>粗</b>**'), '<p><strong>&lt;b&gt;粗&lt;/b&gt;</strong></p>');
  assert.equal(mdLite(''), '');
  assert.equal(mdLite(null), '');
});

// A task's id is the page's own (`t<n>`), but it comes back from localStorage on reload: an edited record can't
// break out of the attributes it is put in
test('a task id from storage stays inside its attributes', () => {
  const pick = re => { const m = html.match(re); assert.ok(m, String(re)); return m[0]; };
  const fns = [/^const esc = .*$/m, /^function turnHtml\([^]*?^}/m].map(pick).join('\n');
  const ctx = {
    TASK_STATUS: { done: { tone: 'ok', icon: 'check', label: 'done' } }, ACTIVE: ['running'], BUILTIN_AGENTS: {},
    taskStatus: s => ({ done: { tone: 'ok', icon: 'check', label: 'done' } })[s],
    S: { expanded: new Set(), pendingDelete: null, agents: [] }, activeTask: () => null, isExternal: () => false,
    icon: () => '', tr: String, hhmm: () => '00:00', mmss: () => '0:00', badge: () => '', mdLite: String,
    agentLabel: () => 'AI', scopeText: () => '', stepHtml: () => '<li></li>',
  };
  const { turnHtml } = runInNewContext(fns + '\n({ turnHtml })', ctx);
  const evil = 't1" onmouseover="alert(1)';
  const out = turnHtml({ id: evil, status: 'done', prompt: 'p', steps: [{}], startedAt: 0, endedAt: 1, source: 'live' });
  assert.ok(!out.includes('" onmouseover='), out);
  assert.ok(out.includes('data-id="t1&quot; onmouseover=&quot;alert(1)"'));
});
