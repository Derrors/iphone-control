import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const html = readFileSync(new URL('../../apps/iphone-control/src/viewer.html', import.meta.url), 'utf8');

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
