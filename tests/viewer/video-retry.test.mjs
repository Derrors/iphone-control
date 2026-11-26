import assert from 'node:assert/strict';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';
import { viewerPage } from './page.mjs';

const html = viewerPage();
const src = html.match(/^const VIDEO_RETRY_MS = .*$/m)[0] + '\n' + html.match(/^function giveUpVideo\([^]*?^}/m)[0]
  + '\n' + html.match(/^function videoEnded\([^]*?^}/m)[0];

function load() {
  const timers = [], ctx = { V: {}, S: { mode: 'live' }, connects: 0, errors: 0 };
  // a timer's id is its place in the list, from 1 (as in a browser, 0 is never one)
  ctx.setTimeout = (f, ms) => timers.push({ f, ms });
  ctx.clearTimeout = id => { if (timers[id - 1]) timers[id - 1].cleared = true; };
  ctx.connect = () => { ctx.connects++; };
  ctx.feedError = () => { ctx.errors++; };
  runInNewContext(src + '\nthis.giveUpVideo = giveUpVideo; this.videoEnded = videoEnded;', ctx);
  return { ctx, timers };
}

test('a picture that never answered is tried again, a few times at most', () => {
  const { ctx, timers } = load();
  for (const [i, ms] of [30000, 60000, 120000].entries()) {
    ctx.giveUpVideo(false, false);
    assert.equal(ctx.V.off, true, 'MJPEG meanwhile');
    const t = timers[i];
    assert.equal(t.ms, ms);
    t.f();
    assert.equal(ctx.V.off, false, 'H.264 again');
  }
  const before = ctx.connects;
  ctx.giveUpVideo(false, false);
  assert.equal(timers.length, 3, 'no more after three');
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

// Gone before its first frame without being refused (a restart, a dropped connection): tried again later, not given
// up for good; refused (404, a codec the browser can't decode): given up; gone after frames: reconnected as it was
test('a picture lost before its first frame is tried again later, one refused is not', () => {
  const { ctx, timers } = load();
  ctx.videoEnded(true, false, false);
  assert.equal(ctx.V.noDevice, true, 'the next kind meanwhile');
  assert.equal(timers.length, 1, 'and this one again later');
  assert.equal(ctx.connects, 1);
  ctx.videoEnded(false, false, true);
  assert.equal(ctx.V.off, true);
  assert.equal(timers.length, 1, 'refused: no retry');
  ctx.videoEnded(false, true, false);
  assert.equal(ctx.errors, 1, 'after frames: reconnected the same way');
});

// One retry waits per kind: giving up again replaces it, so an old one doesn't reconnect out of turn later
test('giving up again replaces the retry that was waiting', () => {
  const { ctx, timers } = load();
  ctx.giveUpVideo(true, false);
  ctx.giveUpVideo(true, false);
  assert.equal(timers.filter(t => !t.cleared).length, 1);
  assert.equal(timers[0].cleared, true);
  ctx.giveUpVideo(true, true);
  assert.equal(timers.filter(t => !t.cleared).length, 0, 'given up for good: none left');
});

// Both kinds given up at once (a restart): the phone's own video comes back first, and the host's retry after it
// doesn't reconnect that working picture
test('the host\'s retry leaves the phone\'s own video, back already, alone', () => {
  const { ctx, timers } = load();
  ctx.S.deviceVideo = true;
  ctx.giveUpVideo(true, false);
  ctx.giveUpVideo(false, false);
  timers[0].f();
  assert.equal(ctx.connects, 1, 'the phone\'s video again');
  timers[1].f();
  assert.equal(ctx.V.off, false, 'the host\'s may be used again');
  assert.equal(ctx.connects, 1, 'but not reconnected for it');
});
