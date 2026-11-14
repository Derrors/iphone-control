import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const html = readFileSync(new URL('../../apps/iphone-control/src/viewer.html', import.meta.url), 'utf8');
const code = html.slice(html.indexOf('<script>'), html.lastIndexOf('</script>'));

// Opened at a network address (`viewer --lan`, plain HTTP) the page isn't a secure context: browsers leave out
// crypto.randomUUID, WebCodecs, the clipboard API. The page must connect without them
test('the picture connects without APIs that need a secure context', () => {
  assert.doesNotMatch(code.replace(/\/\*[^]*?\*\//g, ''), /crypto\.randomUUID\(/, 'crypto.randomUUID is missing on http://<address>');
  const fn = html.match(/^function newFeedId\([^]*?^}/m)[0];
  const { newFeedId } = runInNewContext(fn + '\n({ newFeedId })', { crypto: { getRandomValues: a => { for (let i = 0; i < a.length; i++) a[i] = i * 17; return a; } } });
  assert.match(newFeedId(), /^[0-9a-f]{32}$/);
  assert.ok(newFeedId().length <= 80, 'the viewer accepts feed ids up to 80 characters');
});

test('WebCodecs and the clipboard are only used where they may be missing', () => {
  for (const m of code.matchAll(/new VideoDecoder\(|VideoDecoder\.isConfigSupported/g)) {
    const before = code.slice(Math.max(0, m.index - 6000), m.index);
    assert.match(before, /'VideoDecoder' in window/, 'checked before use');
  }
  const clip = code.indexOf('navigator.clipboard.writeText');
  assert.match(code.slice(clip - 40, clip), /try \{/, 'a failure falls back to selecting the text');
});

// A read-only token (`viewer token add … --read-only`): the page says so and keeps its own hands off; the viewer
// refuses the actions anyway
test('a read-only login hides what it may not do', () => {
  assert.match(html, /\[data-readonly\] #composer[^{]*#dock[^{]*\{ display: none !important; \}/);
  const act = code.match(/^async function act\([^]*?^}/m)[0];
  assert.match(act.split('\n')[1], /S\.readOnly/, 'act stops before sending anything');
  const start = code.match(/^function startTask\([^]*?^}/m)[0];
  assert.match(start, /S\.readOnly/);
  assert.match(code, /S\.readOnly = S\.login\?\.role === 'view'/);
});
