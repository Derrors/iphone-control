import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const html = readFileSync(new URL('../../apps/iphone-control/src/viewer.html', import.meta.url), 'utf8');

test('a pinch (Ctrl/⌘ + wheel) over the picture neither zooms it nor the page', () => {
  const fn = html.match(/^function blockZoomWheel\([^]*?^}/m)[0];
  const { blockZoomWheel } = runInNewContext(fn + '\n({ blockZoomWheel })', {});
  const wheel = mods => { const ev = { ...mods, prevented: false, preventDefault() { this.prevented = true; } }; blockZoomWheel(ev); return ev.prevented; };
  assert.equal(wheel({ ctrlKey: true }), true, 'trackpad pinch');
  assert.equal(wheel({ metaKey: true }), true);
  assert.equal(wheel({}), false, 'plain scrolling still reaches the picture (it swipes the phone)');
});

test('only the zoom buttons zoom the picture', () => {
  const calls = [...html.matchAll(/setZoom\(/g)].map(m => html.slice(html.lastIndexOf('\n', m.index) + 1, html.indexOf('\n', m.index)).trim());
  assert.ok(calls.every(l => !/wheel|gesture|ev\.(deltaY|scale)/.test(l)), calls.join('\n'));
  assert.match(html, /\.stage-wrap \{[^}]*touch-action: pan-x pan-y/, 'touch screens: no pinch zoom around the picture');
  assert.match(html, /addEventListener\(t, ev => ev\.preventDefault\(\)\)/, "Safari's gesture events");
  assert.doesNotMatch(html, /Ctrl\/⌘ 滚动滚轮/, 'the buttons no longer offer it');
});
