import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const html = readFileSync(new URL('../../apps/iphone-control/src/viewer.html', import.meta.url), 'utf8');

// The page's task handlers, with the network and rendering replaced (same approach as actions.test.mjs)
function handlers(names, context) {
  const functions = names.map(name => {
    const match = html.match(new RegExp(`^(?:async )?function ${name}\\([^]*?^}`, 'm'));
    assert.ok(match, name);
    return match[0];
  });
  return runInNewContext(functions.join('\n') + '\n({' + names.join(', ') + '})', context);
}

const ACTIVE = ['running', 'paused', 'takeover', 'waiting'];
const noop = () => {};

function page(tasks, serverTasks) {
  const S = { mode: 'live', tasks, seq: tasks.length, expanded: new Set() };
  const followed = [];
  const context = {
    S, ACTIVE, adopting: false, tr: String, show: noop, renderAll: noop, scrollConvo: noop,
    activeTask: () => S.tasks.find(t => ACTIVE.includes(t.status)),
    follow: (task, id) => { task.remoteId = id; followed.push([task, id]); },
    calls: 0,
    async api(path) { context.calls++; assert.equal(path, '/api/agent/tasks'); return { tasks: serverTasks }; },
  };
  return { S, followed, context, ...handlers(['lostTask', 'adoptTask'], context) };
}

test('a task that was running before the reload is followed again', async () => {
  const mine = { id: 't1', status: 'running', steps: [{ id: 1, status: 'running' }], remoteId: 'abc', reattach: true };
  const { followed, adoptTask } = page([mine], [{ id: 'abc', state: 'paused', title: 'x', owner: 'claude' }]);
  await adoptTask();
  assert.deepEqual(followed.map(([t, id]) => [t.id, id]), [['t1', 'abc']]);
  assert.equal(mine.reattach, false);
  assert.equal(mine.status, 'running', 'the replayed events set the state');
});

test('a task the daemon no longer knows is marked stopped', async () => {
  const mine = { id: 't1', status: 'waiting', steps: [{ id: 1, status: 'waiting' }], remoteId: 'gone', reattach: true };
  const { followed, adoptTask } = page([mine], []);
  await adoptTask();
  assert.equal(followed.length, 0);
  assert.equal(mine.status, 'stopped');
  assert.equal(mine.steps[0].status, 'stopped');
});

test('a task started elsewhere shows up with its title and who runs it', async () => {
  const done = { id: 't1', status: 'success', steps: [], remoteId: 'old' };
  const server = [{ id: 'old', state: 'completed' }, { id: 'ext', state: 'running', title: '查天气', owner: 'happyclaw', started: 5 }];
  const { S, followed, adoptTask } = page([done], server);
  await adoptTask();
  assert.equal(S.tasks.length, 2);
  const t = S.tasks[1];
  assert.deepEqual([t.prompt, t.agent, t.status, t.startedAt], ['查天气', 'happyclaw', 'running', 5]);
  assert.deepEqual(followed.map(([, id]) => id), ['ext']);
  await adoptTask();
  assert.equal(S.tasks.length, 2, 'only once');
});

test('nothing to ask while this page follows its own task', async () => {
  const mine = { id: 't1', status: 'running', steps: [], remoteId: 'abc' };
  const { context, adoptTask } = page([mine], []);
  await adoptTask();
  assert.equal(context.calls, 0);
});
