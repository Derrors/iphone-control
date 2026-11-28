import assert from 'node:assert/strict';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';
import { viewerPage } from './page.mjs';

const html = viewerPage();

function loadStateEvents(context) {
  const vars = html.match(/^let stateEvents = null, stateEventsOpen = false;$/m);
  assert.ok(vars, 'stateEvents declarations');
  const names = ['stopStateEvents', 'startStateEvents', 'pollInfo'];
  const functions = names.map(name => {
    const match = html.match(new RegExp(`^(?:async )?function ${name}\\([^]*?^}`, 'm'));
    assert.ok(match, name);
    return match[0];
  });
  return runInNewContext(vars[0] + '\n' + functions.join('\n') + '\n({' + names.join(', ') + '})', context);
}

test('state SSE stream applies info/daemon/status/tasks and pauses polling while open', async () => {
  const made = [];
  class FakeES {
    constructor(url) { this.url = url; this.on = {}; this.closed = false; made.push(this); }
    addEventListener(name, f) { this.on[name] = f; }
    close() { this.closed = true; }
  }
  const applied = { info: [], daemon: [], status: [], tasks: [], polls: 0 };
  const context = {
    S: { mode: 'live' },
    document: { hidden: false },
    EventSource: FakeES,
    applyInfo: v => applied.info.push(v),
    applyDaemonState: v => applied.daemon.push(v),
    applyStatusState: v => applied.status.push(v),
    wantsTasks: () => true,
    adoptTask: v => applied.tasks.push(v),
    loadInfo: async () => { applied.polls++; },
    setTimeout: () => 0,
  };
  const { startStateEvents, stopStateEvents, pollInfo } = loadStateEvents(context);

  startStateEvents();
  assert.equal(made.length, 1);
  assert.equal(made[0].url, '/api/events');

  made[0].on.open();
  made[0].on.info({ data: JSON.stringify({ udid: 'u1', state: 'Ready' }) });
  made[0].on.daemon({ data: JSON.stringify({ state: 'Ready', devices: [] }) });
  made[0].on.status({ data: JSON.stringify({ locked: false, route: 'native' }) });
  made[0].on.tasks({ data: JSON.stringify([{ id: 't1', state: 'running' }]) });
  assert.equal(applied.info.length, 1);
  assert.equal(applied.daemon.length, 1);
  assert.equal(applied.status.length, 1);
  assert.equal(applied.tasks.length, 1);

  await pollInfo();
  assert.equal(applied.polls, 0, 'polling paused while SSE stream is open');

  made[0].on.error();
  await pollInfo();
  assert.equal(applied.polls, 1, 'polling resumes after SSE error');

  stopStateEvents();
  assert.equal(made[0].closed, true);
});
