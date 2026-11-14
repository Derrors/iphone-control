import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const html = readFileSync(new URL('../../apps/iphone-control/src/viewer.html', import.meta.url), 'utf8');

test('low-bandwidth mode is kept in the browser and asks the viewer for it with the picture', () => {
  const fn = html.match(/^const LOW_BANDWIDTH_KEY = .*\n^function lowBandwidthOn\(\)[^\n]*$/m)[0];
  const store = {};
  const localStorage = { getItem: k => store[k] ?? null };
  const { lowBandwidthOn } = runInNewContext(fn + '\n({ lowBandwidthOn })', { localStorage });
  assert.equal(lowBandwidthOn(), false, 'off unless turned on');
  store['ictl.lowBandwidth'] = '1';
  assert.equal(lowBandwidthOn(), true);
  const broken = runInNewContext(fn + '\n({ lowBandwidthOn })', { localStorage: { getItem() { throw new Error('denied'); } } });
  assert.equal(broken.lowBandwidthOn(), false, 'storage turned off');
  // one request for every kind of picture: the phone's own HEVC and the H.264
  const fetches = [...html.matchAll(/fetch\(`\/video\?[^`]*`/g)].map(m => m[0]);
  assert.equal(fetches.length, 1, fetches.join('\n'));
  assert.match(fetches[0], /lowBandwidthOn\(\) \? '&low=1' : ''/);
});

test('the switch is in the settings, says what it costs, and is translated', () => {
  assert.match(html, /<dialog[^>]*id="appearance"[^]*?id="low-bandwidth"[^]*?<\/dialog>/);
  const hint = html.match(/<p class="appearance-hint" id="low-bandwidth-hint">([^<]+)<\/p>/)[1];
  assert.match(hint, /所有在看这台手机的页面/, 'everyone watching that phone gets it');
  const EN = JSON.parse(html.match(/^const EN = (\{.*\});$/m)[1]);
  for (const zh of ['弱网模式', hint, '已开启弱网模式，正在重新连接画面…', '已关闭弱网模式，正在重新连接画面…']) {
    assert.ok(EN[zh], zh);
  }
  // its own change handler: not saved as an appearance setting
  assert.match(html, /\$\('#low-bandwidth'\)\.addEventListener\('change', ev => \{\n  ev\.stopPropagation\(\);/);
});
