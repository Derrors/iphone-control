import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const html = readFileSync(new URL('../../apps/iphone-control/src/viewer.html', import.meta.url), 'utf8');
const src = html.match(/^const VIDEO_RETRY_MS = .*$/m)[0] + '\n' + html.match(/^function giveUpVideo\([^]*?^}/m)[0];

function load() {
  const timers = [], ctx = { V: {}, S: { mode: 'live' }, connects: 0 };
  ctx.setTimeout = (f, ms) => timers.push({ f, ms });
  ctx.connect = () => { ctx.connects++; };
  runInNewContext(src + '\nthis.giveUpVideo = giveUpVideo;', ctx);
  return { ctx, timers };
}

test('a picture that never answered is tried again, a few times at most', () => {
  const { ctx, timers } = load();
  for (const ms of [30000, 60000, 120000]) {
    ctx.giveUpVideo(false, false);
    assert.equal(ctx.V.off, true, 'MJPEG meanwhile');
    const t = timers.shift();
    assert.equal(t.ms, ms);
    t.f();
    assert.equal(ctx.V.off, false, 'H.264 again');
  }
  const before = ctx.connects;
  ctx.giveUpVideo(false, false);
  assert.equal(timers.length, 0, 'no more after three');
  assert.equal(ctx.V.off, true);
  assert.equal(ctx.connects, before);
});

test('a picture the viewer refused is not tried again; the phone\'s own video counts its own tries', () => {
  const { ctx, timers } = load();
  ctx.giveUpVideo(false, true);
  assert.equal(ctx.V.off, true);
  assert.equal(timers.length, 0);
  ctx.giveUpVideo(true, false);
  assert.equal(ctx.V.noDevice, true);
  assert.equal(timers[0].ms, 30000, 'its first retry');
  timers[0].f();
  assert.equal(ctx.V.noDevice, false);
  assert.equal(ctx.connects, 1);
});
