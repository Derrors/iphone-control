import assert from 'node:assert/strict';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';
import { viewerPage } from './page.mjs';

const html = viewerPage();

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
  const S = { mode: 'live', tasks, seq: tasks.length, expanded: new Set(), agents: [] };
  const followed = [], shown = [];
  const context = {
    S, ACTIVE, adopting: false, tr: String, show: m => shown.push(m), renderAll: noop, scrollConvo: noop,
    BUILTIN_AGENTS: { claude: 'Claude Code', codex: 'Codex' },
    activeTask: () => S.tasks.find(t => ACTIVE.includes(t.status)),
    follow: (task, id) => { task.remoteId = id; followed.push([task, id]); },
    calls: 0,
    async api(path) { context.calls++; assert.equal(path, '/api/agent/tasks'); return { tasks: serverTasks }; },
  };
  return { S, followed, shown, context, ...handlers(['isExternal', 'lostTask', 'wantsTasks', 'adoptTask'], context) };
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
  const { S, followed, shown, adoptTask } = page([done], server);
  await adoptTask();
  assert.equal(S.tasks.length, 2);
  assert.deepEqual(shown, ['接上了一个正在进行的外部任务'], 'not called an AI task');
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

function who(agents = []) {
  const S = { agents, tasks: [] };
  const context = { S, tr: String };
  runInNewContext(html.match(/^const BUILTIN_AGENTS = .*$/m)[0] + '\nthis.BUILTIN_AGENTS = BUILTIN_AGENTS;', context);
  return { S, ...handlers(['isExternal', 'agentLabel'], context) };
}

test('each task shows who runs it; a task opened elsewhere is marked external', () => {
  const { isExternal, agentLabel } = who([{ id: 'claude', name: 'Claude Code', available: true }]);
  const live = agent => ({ source: 'live', agent });
  assert.equal(agentLabel(live('claude')), 'Claude Code');
  assert.equal(agentLabel(live('codex')), 'Codex', 'known even before the backends are read');
  assert.equal(agentLabel(live('happyclaw')), 'happyclaw · 外部任务');
  assert.equal(agentLabel({ source: 'demo', agent: 'claude' }), 'AI 助手');
  assert.equal(agentLabel({ source: 'live' }), 'AI 助手', 'records saved before the agent was kept');
  assert.deepEqual([live('claude'), live('codex'), live('external'), { source: 'live' }].map(isExternal), [false, false, true, false]);
});

test('a task opened elsewhere is never run again as a prompt', () => {
  const { S } = who();
  const started = [];
  const context = { S, tr: String, show: () => {}, BUILTIN_AGENTS: { claude: 'Claude Code', codex: 'Codex' }, activeTask: () => null, startTask: p => { started.push(p); return true; } };
  const { rerunTask } = handlers(['isExternal', 'rerunTask'], context);
  S.tasks = [{ id: 't1', source: 'live', agent: 'external', prompt: '外部任务测试' }, { id: 't2', source: 'live', agent: 'claude', prompt: '打开设置' }];
  rerunTask('t1');
  rerunTask('t2');
  assert.deepEqual(started, ['打开设置']);
});

// The list comes with the page's /api/info poll (`?tasks=1` while it wants one): no request of its own then
test('a task list from the info poll is used as it is', async () => {
  const { followed, context, wantsTasks, adoptTask } = page([], []);
  assert.equal(wantsTasks(), true, 'no task followed: the poll asks for the list');
  await adoptTask([{ id: 'xyz', state: 'running', title: 'from elsewhere', owner: 'external' }]);
  assert.equal(context.calls, 0, 'no request of its own');
  assert.deepEqual(followed.map(([, id]) => id), ['xyz']);
  assert.equal(wantsTasks(), false, 'following one now: the poll stops asking');
});

// While a step waits for the user they can take over first; the question stays and can be answered then or after
test('a step waiting for the user can be answered after taking over', () => {
  const step = { id: 3, status: 'waiting', confirm: { action: '发送消息' } };
  const t = { id: 't1', source: 'live', status: 'waiting', steps: [{ id: 1, status: 'ok' }, step] };
  const S = { tasks: [t] }, sent = [], shown = [];
  const context = {
    S, ACTIVE, tr: String, renderAll: noop, show: m => shown.push(m),
    activeTask: () => S.tasks.find(x => ACTIVE.includes(x.status)),
    liveControl: (task, action, body) => sent.push([action, body]),
  };
  const { control, answerConfirm } = handlers(['pendingConfirm', 'control', 'answerConfirm'], context);
  control('takeover');
  assert.equal(t.status, 'takeover');
  control('handback');
  assert.equal(t.status, 'waiting', 'handed back with the question unanswered: waiting again');
  control('takeover');
  answerConfirm(true);
  assert.equal(JSON.stringify(sent.at(-1)), JSON.stringify(['confirm', { step: 3, approve: true }]));
  assert.equal(step.status, 'running');
  assert.equal(t.status, 'takeover', 'answered, but still the user\'s until handed back');
  assert.equal(shown.at(-1), '已批准，交还 AI 后继续执行', 'says it goes on only once handed back');
  control('handback');
  assert.equal(t.status, 'running');
});

// The event stream replays the whole task on each connection: a step's mark is drawn once, not again on reconnecting
function streams() {
  const made = [];
  class FakeES {
    static CLOSED = 2;
    constructor(url) { this.url = url; this.on = {}; this.readyState = 0; made.push(this); }
    addEventListener(name, f) { this.on[name] = f; }
    close() { this.readyState = 2; }
    emit(name, data) { this.on[name]({ data: JSON.stringify(data) }); }
  }
  return { made, FakeES };
}

test('a step mark is drawn once however often the stream replays it', () => {
  const { made, FakeES } = streams();
  const marks = [];
  const task = { id: 't1', status: 'running', steps: [] };
  const context = { ACTIVE, EventSource: FakeES, tr: String, show: noop, renderAll: noop, renderTurns: noop, scrollConvo: noop, refreshStatusSoon: noop,
    drawMark: (...a) => marks.push(a), finish: noop, streamClosed: noop };
  const { follow } = handlers(['upsertStep', 'follow'], context);
  const tap = { id: 1, tool: 'iphone_tap', status: 'running', gesture: { kind: 'tap', x: 1, y: 2 } };
  follow(task, 'abc');
  made[0].emit('step', tap);
  follow(task, 'abc');                       // reconnected: the same step replayed
  made[1].emit('step', tap);
  made[1].emit('step', { ...tap, id: 2 });
  assert.equal(marks.length, 2, 'step 1 once, step 2 once');
});

// The steps saved before a reload keep their gestures, so the replayed stream doesn't draw them again
test('a mark drawn before a reload is not drawn again after it', () => {
  const { made, FakeES } = streams();
  const marks = [];
  const S = { tasks: [{ id: 't1', status: 'running', remoteId: 'abc', steps: [] }] };
  const context = { S, MAX_TASKS: 50, ACTIVE, EventSource: FakeES, tr: String, show: noop, renderAll: noop, renderTurns: noop, scrollConvo: noop,
    refreshStatusSoon: noop, drawMark: (...a) => marks.push(a), finish: noop, streamClosed: noop };
  const { follow, serializeTasks } = handlers(['upsertStep', 'follow', 'serializeTasks'], context);
  const tap = { id: 1, tool: 'iphone_tap', status: 'ok', gesture: { kind: 'tap', x: 1, y: 2 } };
  follow(S.tasks[0], 'abc');
  made[0].emit('step', tap);
  assert.equal(marks.length, 1);
  const [saved] = JSON.parse(serializeTasks(false));   // the page reloads
  follow(saved, 'abc');
  made[1].emit('step', tap);
  assert.equal(marks.length, 1, 'known before the reload');
  made[1].emit('step', { ...tap, id: 2 });
  assert.equal(marks.length, 2, 'a new one is drawn');
});

// A task that ends leaves no step spinning: an older daemon sends done with a step still open
test('steps still open when a task ends are stopped', () => {
  const task = { id: 't1', status: 'running', steps: [{ id: 1, status: 'ok' }, { id: 2, status: 'running' }, { id: 3, status: 'waiting' }] };
  const context = { S: { expanded: new Set() }, tr: String, show: noop, renderAll: noop, scrollConvo: noop };
  const { finish } = handlers(['finish'], context);
  finish(task, 'stopped', '');
  assert.deepEqual(task.steps.map(s => s.status), ['ok', 'stopped', 'stopped']);
});

test('an unknown task state is shown as it is and does not replace a known one', () => {
  const { made, FakeES } = streams();
  const task = { id: 't1', status: 'running', steps: [] };
  const context = { ACTIVE, EventSource: FakeES, tr: String, show: noop, renderAll: noop, renderTurns: noop, scrollConvo: noop, refreshStatusSoon: noop, drawMark: noop, finish: noop, streamClosed: noop };
  const { follow } = handlers(['upsertStep', 'follow'], context);
  follow(task, 'abc');
  made[0].emit('state', { state: 'something-new' });
  assert.equal(task.status, 'running');
  made[0].emit('state', { state: 'paused' });
  assert.equal(task.status, 'paused');
  const state = html.match(/^function taskStatus\([^]*?^}/m) || html.match(/^function taskStatus.*$/m);
  const ts = runInNewContext(html.match(/^const TASK_STATUS = \{[^]*?^\};/m)[0] + '\n' + state[0] + '\ntaskStatus', { tr: String });
  assert.equal(ts('something-new').label, 'something-new');
  assert.equal(ts('paused').label, '已暂停');
});

test('a stream the browser gave up on is followed again if the task is still there, else marked stopped', async () => {
  const { made, FakeES } = streams();
  for (const [server, want] of [[[{ id: 'abc', state: 'running' }], 'followed'], [[], 'stopped']]) {
    made.length = 0;
    const task = { id: 't1', status: 'running', steps: [{ id: 1, status: 'running' }], remoteId: 'abc' };
    const context = { ACTIVE, EventSource: FakeES, tr: String, show: noop, renderAll: noop, renderTurns: noop, scrollConvo: noop, refreshStatusSoon: noop, drawMark: noop, finish: noop,
      sleep: async () => {}, api: async () => ({ tasks: server }) };
    const h = handlers(['upsertStep', 'follow', 'lostTask', 'streamClosed'], context);
    runInNewContext(html.match(/^const REFOLLOW_MS = .*$/m)[0] + '\n' + html.match(/^const LOST_STREAM = .*$/m)[0] + '\nthis.REFOLLOW_MS = REFOLLOW_MS; this.REFOLLOW_TRIES = REFOLLOW_TRIES; this.LOST_STREAM = LOST_STREAM;', context);
    context.follow = h.follow; context.lostTask = h.lostTask; context.streamClosed = h.streamClosed;
    await h.streamClosed(task, 'abc');
    assert.equal(want === 'followed' ? made.length : task.status, want === 'followed' ? 1 : 'stopped', want);
  }
});
