"""Real-device scenarios for AI tasks kept by the daemon (docs/task-control.md; regression cases #65–67, #106–110).

    pip install -r tests/device/requirements.txt
    python tests/device/tasks.py                  # every scenario except `restart`
    python tests/device/tasks.py pause scope      # only these
    python tests/device/tasks.py restart --restart 'systemctl --user restart iphone-control'

It plays an agent platform: opens tasks on the daemon's control socket (holding the lease the way `task run` does),
starts one MCP server with the task's token (the agent) and one without (anyone else), steers the task with the
`iphone-control task …` commands and checks what the daemon lets through. Every check prints a PASS / FAIL line with
what came back; the last line sums up, and the exit code is 1 if anything failed. Paste the output into the pull
request (CONTRIBUTING.md, "Required tests", part 4).

Harmless by design: it presses Home, opens Settings and Clock, takes screenshots, and refuses the two links it tries
to open (one of them a scheme no app handles). Nothing is sent, bought, deleted or changed.

Environment: ICTL_BIN (default `iphone-control`; your branch's build for a pull request), MCP_CMD (default
`$ICTL_BIN mcp`), ICTL_SOCKET (default `$XDG_RUNTIME_DIR/iphone-control/ctl.sock`).
"""

import argparse
import asyncio
import json
import os
import shlex
import sys
import tempfile
import time
from contextlib import AsyncExitStack

from mcp import ClientSession
from mcp.client.stdio import StdioServerParameters, stdio_client

BIN = shlex.split(os.environ.get("ICTL_BIN", "iphone-control"))
MCP_CMD = shlex.split(os.environ.get("MCP_CMD", "")) or [*BIN, "mcp"]
SOCKET = os.environ.get("ICTL_SOCKET") or os.path.join(
    os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"), "iphone-control", "ctl.sock"
)
TOKEN_ENV = "IPHONE_CONTROL_TASK_TOKEN"
OWNER = "device-test"
HERE = os.path.dirname(os.path.abspath(__file__))

results: list[bool] = []


def short(text: str, n: int = 160) -> str:
    one = " ".join(text.split())
    return one if len(one) <= n else one[: n - 1] + "…"


def check(case: str, what: str, ok: bool, got: str = "") -> bool:
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'} [{case}] {what}" + (f"  ← {short(got)}" if got else ""), flush=True)
    return ok


# ---------------------------------------------------------------- the daemon's control socket


async def ask(req: dict, timeout: float = 15) -> dict:
    r, w = await asyncio.open_unix_connection(SOCKET)
    try:
        w.write((json.dumps(req, ensure_ascii=False) + "\n").encode())
        await w.drain()
        return json.loads(await asyncio.wait_for(r.readline(), timeout))
    finally:
        w.close()


async def tasks() -> list[dict]:
    r = await ask({"op": "task_list"})
    if r.get("kind") != "text":
        raise RuntimeError(f"task_list: {r}")
    return json.loads(r["text"])


async def info(task_id: str) -> dict:
    return next((t for t in await tasks() if t["id"] == task_id), {})


async def wait_for(cond, timeout: float = 15, every: float = 0.2):
    """Polls the async `cond()` until it returns something truthy; returns that, or None on timeout"""
    end = time.time() + timeout
    while time.time() < end:
        v = await cond()
        if v:
            return v
        await asyncio.sleep(every)
    return None


async def state_is(task_id: str, state: str, timeout: float = 15) -> bool:
    async def ok():
        return (await info(task_id)).get("state") == state

    return bool(await wait_for(ok, timeout))


class Lease:
    """An opened task: the connection is its lease, and its events are collected as they come"""

    def __init__(self):
        self.id = self.token = ""
        self.events: list[dict] = []
        self._w = None
        self._reader = None

    @classmethod
    async def open(cls, title: str, apps=()):
        self = cls()
        r, self._w = await asyncio.open_unix_connection(SOCKET)
        req = {"op": "task_open", "title": title, "owner": OWNER, "apps": list(apps)}
        self._w.write((json.dumps(req, ensure_ascii=False) + "\n").encode())
        await self._w.drain()
        first = json.loads(await asyncio.wait_for(r.readline(), 60))
        if first.get("kind") != "text":
            self._w.close()
            raise RuntimeError(first.get("message", first))
        opened = json.loads(first["text"])
        self.id, self.token = opened["id"], opened["token"]
        self._reader = asyncio.create_task(self._read(r))
        return self

    async def _read(self, r):
        while line := await r.readline():
            try:
                self.events.append(json.loads(line))
            except ValueError:
                pass

    def of(self, kind: str) -> list[dict]:
        return [e["data"] for e in self.events if e["event"] == kind]

    def done(self):
        return next(iter(self.of("done")), None)

    async def finish(self, ok: bool = True, summary: str = "device test"):
        return await ask({"op": "task_finish", "ok": ok, "summary": summary, "task": self.token})

    def close(self):
        if self._w:
            self._w.close()
            self._w = None


# ---------------------------------------------------------------- MCP servers and the CLI


async def mcp(stack: AsyncExitStack, token: str | None) -> ClientSession:
    env = dict(os.environ)
    env.pop(TOKEN_ENV, None)
    if token:
        env[TOKEN_ENV] = token
    params = StdioServerParameters(command=MCP_CMD[0], args=MCP_CMD[1:], env=env)
    r, w = await stack.enter_async_context(stdio_client(params))
    s = await stack.enter_async_context(ClientSession(r, w))
    await s.initialize()
    return s


async def call(s: ClientSession, name: str, args: dict | None = None, timeout: float = 90):
    """(is_error, text, seconds)"""
    t = time.time()
    res = await asyncio.wait_for(s.call_tool(name, args or {}), timeout)
    text = "\n".join(c.text for c in res.content if c.type == "text")
    return bool(res.is_error), text, time.time() - t


async def cli(*args: str, timeout: float = 30):
    """`iphone-control task …`: (exit code, output)"""
    p = await asyncio.create_subprocess_exec(
        *BIN, "task", *args, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
    )
    out, _ = await asyncio.wait_for(p.communicate(), timeout)
    return p.returncode, out.decode(errors="replace").strip()


async def confirm_asked(lease: Lease, before: int, timeout: float = 30):
    """Waits until the task waits for a confirmation it didn't have before; returns the confirmation"""

    async def asked():
        c = lease.of("confirm")
        return c[-1] if len(c) > before and (await info(lease.id)).get("state") == "waiting" else None

    return await wait_for(asked, timeout)


# ---------------------------------------------------------------- scenarios


async def others(stack):
    """#106: while a task runs, anyone else may look but not act; the task's agent may"""
    c = "others"
    lease = await Lease.open("真机回归：任务进行中别人只能看")
    stack.callback(lease.close)
    agent, other = await mcp(stack, lease.token), await mcp(stack, None)
    names = lambda r: {t.name for t in r.tools}  # noqa: E731
    check(c, "the agent's MCP server lists iphone_confirm", "iphone_confirm" in names(await agent.list_tools()))
    check(c, "a plain MCP server doesn't", "iphone_confirm" not in names(await other.list_tools()))
    err, text, dt = await call(other, "iphone_screenshot")
    check(c, f"another caller may take a screenshot ({dt:.2f}s)", not err, text)
    err, text, dt = await call(other, "iphone_press_button", {"button": "home"})
    check(c, f"another caller's Home is refused, naming the task ({dt:.2f}s)", err and lease.id in text, text)
    # the device it already controls: harmless even if it went through (`auto` would undo a pinned choice)
    udid = (await info(lease.id)).get("udid", "")
    err, text, _ = await call(other, "iphone_select_device", {"device": udid})
    check(c, "selecting a device is refused", err, text)
    err, text, dt = await call(agent, "iphone_press_button", {"button": "home"})
    check(c, f"the agent's Home goes through ({dt:.2f}s)", not err, text)
    steps = [s for s in lease.of("step") if s.get("tool") == "iphone_press_button"]
    check(c, "the agent's call is a step of the task", bool(steps), json.dumps(steps[:1], ensure_ascii=False))
    await lease.finish()
    check(c, "finished as completed", await state_is(lease.id, "completed"))
    err, text, dt = await call(other, "iphone_press_button", {"button": "home"})
    check(c, f"after the task, anyone may act again ({dt:.2f}s)", not err, text)


async def pause(stack):
    """#66: a paused task's calls wait in the daemon, also between the steps of iphone_batch"""
    c = "pause"
    lease = await Lease.open("真机回归：暂停")
    stack.callback(lease.close)
    agent = await mcp(stack, lease.token)
    code, out = await cli("pause", lease.id)
    check(c, "`task pause` works", code == 0, out)
    began = lambda: len([s for s in lease.of("step") if "tool" in s])  # noqa: E731
    n = began()
    held = asyncio.create_task(call(agent, "iphone_screenshot"))
    await asyncio.sleep(5)
    check(c, "a call while paused is held (5 s, no result)", not held.done())
    check(c, "and no new step shows up", began() == n)
    code, out = await cli("resume", lease.id)
    check(c, "`task resume` works", code == 0, out)
    err, text, dt = await asyncio.wait_for(held, 60)
    check(c, f"after resuming it runs ({dt:.1f}s in all)", not err and dt >= 5, text)

    steps = [{"tool": "iphone_press_button", "button": "home"}, {"tool": "wait", "seconds": 3}, {"tool": "iphone_press_button", "button": "home"}]
    batch = asyncio.create_task(call(agent, "iphone_batch", {"steps": steps}))
    await asyncio.sleep(1.5)
    await cli("pause", lease.id)
    await asyncio.sleep(5)
    check(c, "a batch paused in its wait doesn't take its next step (held 5 s after a 3 s wait)", not batch.done())
    await cli("resume", lease.id)
    err, text, dt = await asyncio.wait_for(batch, 60)
    check(c, f"after resuming the batch finishes ({dt:.1f}s in all)", not err and dt >= 6, text)
    await lease.finish()


async def handback(stack):
    """#67: the user may act while the task is taken over; after handing back, the agent looks before acting"""
    c = "handback"
    lease = await Lease.open("真机回归：接管和交还")
    stack.callback(lease.close)
    agent, other = await mcp(stack, lease.token), await mcp(stack, None)
    code, out = await cli("takeover", lease.id)
    check(c, "`task takeover` works", code == 0 and await state_is(lease.id, "takeover"), out)
    err, text, dt = await call(other, "iphone_press_button", {"button": "home"})
    check(c, f"while taken over, the user's Home goes through ({dt:.2f}s)", not err, text)
    code, out = await cli("handback", lease.id)
    check(c, "`task handback` works", code == 0 and await state_is(lease.id, "running"), out)
    err, text, _ = await call(agent, "iphone_press_button", {"button": "home"})
    check(c, "the agent's first action is refused: look first", err and "iphone_screenshot" in text, text)
    err, text, _ = await call(agent, "iphone_press_button", {"button": "home"})
    check(c, "and the next one too, until it has looked", err, text)
    err, text, _ = await call(agent, "iphone_screenshot")
    check(c, "a screenshot goes through", not err, text)
    err, text, _ = await call(agent, "iphone_press_button", {"button": "home"})
    check(c, "after it, the agent acts again", not err, text)
    await lease.finish()


async def confirm(stack):
    """iphone_confirm waits for `task approve` / `task reject`; a rejection ends the task as rejected"""
    c = "confirm"
    lease = await Lease.open("真机回归：确认")
    stack.callback(lease.close)
    agent = await mcp(stack, lease.token)
    code, out = await cli("approve", lease.id)
    check(c, "`task approve` with nothing waiting fails", code != 0, out)
    args = {"action": "真机回归：测试确认", "detail": "只是测试，同意或拒绝都不会在手机上做任何事"}
    for approve in (True, False):
        before = len(lease.of("confirm"))
        asking = asyncio.create_task(call(agent, "iphone_confirm", args))
        shown = await confirm_asked(lease, before)
        check(c, "the task waits, with the confirmation shown", bool(shown), json.dumps(shown, ensure_ascii=False))
        code, out = await cli("approve" if approve else "reject", lease.id)
        err, text, dt = await asyncio.wait_for(asking, 30)
        what = "approved: a normal result" if approve else "rejected: an error the agent must stop on"
        check(c, f"{what} ({dt:.1f}s)", code == 0 and err != approve, text)
    await lease.finish(ok=True)
    check(c, "the task ends as rejected though the agent said ok", await state_is(lease.id, "rejected"))


async def stop(stack):
    """#65: once stopped, the task's token is refused for everything"""
    c = "stop"
    lease = await Lease.open("真机回归：停止")
    stack.callback(lease.close)
    agent = await mcp(stack, lease.token)
    err, text, _ = await call(agent, "iphone_press_button", {"button": "home"})
    check(c, "the agent acts", not err, text)
    code, out = await cli("stop", lease.id)
    check(c, "`task stop` works", code == 0, out)
    done = await wait_for(lambda: asyncio.sleep(0, lease.done()), 10)
    check(c, "the opener hears it ended (cancelled)", bool(done) and done.get("outcome") == "cancelled", json.dumps(done, ensure_ascii=False))
    for tool in ("iphone_press_button", "iphone_screenshot"):
        err, text, _ = await call(agent, tool, {"button": "home"} if tool == "iphone_press_button" else {})
        check(c, f"afterwards the agent's {tool} is refused", err, text)


async def lease(stack):
    """#65: the opener going away ends the task as abandoned; the agent stops, others may act again"""
    c = "lease"
    t = await Lease.open("真机回归：发起方断开")
    agent, other = await mcp(stack, t.token), await mcp(stack, None)
    t.close()
    check(c, "closing the lease ends the task as abandoned", await state_is(t.id, "abandoned", 10))
    err, text, _ = await call(agent, "iphone_screenshot")
    check(c, "the agent is refused", err, text)
    err, text, _ = await call(other, "iphone_press_button", {"button": "home"})
    check(c, "others may act again", not err, text)


async def run_cmd(stack):
    """`task run`: the integration for agent platforms (#50), end to end and when stopped from elsewhere"""
    c = "run"
    title = f"真机回归：task run {int(time.time())}"
    steps = [
        ["iphone_status", {}],
        ["iphone_press_button", {"button": "home"}],
        ["iphone_launch_app", {"app": "时钟"}],
        ["iphone_list_elements", {"limit": 5}],
        ["iphone_press_button", {"button": "home"}],
    ]
    env = {**os.environ, "MCP_CMD": shlex.join(MCP_CMD), "SHOTS_DIR": tempfile.mkdtemp(prefix="ictl-shots-")}
    env.pop(TOKEN_ENV, None)
    p = await asyncio.create_subprocess_exec(
        *BIN, "task", "run", "--title", title, "--owner", OWNER, "--",
        sys.executable, os.path.join(HERE, "run.py"), json.dumps(steps, ensure_ascii=False),
        env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
    )
    out, _ = await asyncio.wait_for(p.communicate(), 300)
    out = out.decode(errors="replace")
    last = [line for line in out.splitlines() if line.startswith("---")]
    check(c, "run.py under `task run` passes (exit 0)", p.returncode == 0, last[-1] if last else out[-300:])
    t = next((t for t in reversed(await tasks()) if t["title"] == title), {})
    check(c, "its task completed with every call as a step", t.get("state") == "completed" and t.get("steps", 0) >= len(steps), json.dumps(t, ensure_ascii=False))

    title = f"真机回归：task run 被停止 {int(time.time())}"
    p = await asyncio.create_subprocess_exec(
        *BIN, "task", "run", "--title", title, "--owner", OWNER, "--", "sleep", "600",
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
    )

    async def running():
        return next((t for t in await tasks() if t["title"] == title and t["state"] == "running"), None)

    t = await wait_for(running, 20)
    check(c, "`task run -- sleep 600` opens a task", bool(t))
    if t:
        t0 = time.time()
        await cli("stop", t["id"])
        code = await asyncio.wait_for(p.wait(), 15)
        check(c, f"`task stop` ends the command too ({time.time() - t0:.1f}s, exit {code})", code != 0)
        check(c, "and the task is cancelled", await state_is(t["id"], "cancelled"))
    elif p.returncode is None:
        p.kill()


async def scope(stack):
    """#110: a task limited to Settings asks before going elsewhere; approving lets it in, rejecting refuses one step"""
    c = "scope"
    lease = await Lease.open("真机回归：只在设置里", apps=["设置"])
    stack.callback(lease.close)
    apps = (await info(lease.id)).get("apps", [])
    if not apps:
        print(f"SKIP [{c}] this daemon has no app scope (feat/task-scope)", flush=True)
        await lease.finish()
        return
    check(c, "「设置」 resolved to its bundle ID", "com.apple.Preferences" in apps, ", ".join(apps))
    agent = await mcp(stack, lease.token)
    asked = lambda: len(lease.of("confirm"))  # noqa: E731

    n = asked()
    err, text, _ = await call(agent, "iphone_launch_app", {"app": "设置"})
    check(c, "launching Settings doesn't ask", not err and asked() == n, text)

    async def outside(name, args, approve):
        n = asked()
        doing = asyncio.create_task(call(agent, name, args))
        shown = await confirm_asked(lease, n)
        check(c, f"{name} {json.dumps(args, ensure_ascii=False)} waits for the user", bool(shown), json.dumps(shown, ensure_ascii=False))
        await cli("approve" if approve else "reject", lease.id)
        err, text, dt = await asyncio.wait_for(doing, 60)
        check(c, f"…{'approved: done' if approve else 'rejected: refused, the task goes on'} ({dt:.1f}s)", err != approve, text)
        return shown

    shown = await outside("iphone_launch_app", {"app": "时钟"}, False)
    check(c, "the question names the app", bool(shown) and "mobiletimer" in json.dumps(shown))
    check(c, "the task is still running", (await info(lease.id)).get("state") == "running")
    err, text, _ = await call(agent, "iphone_press_button", {"button": "home"})
    check(c, "Home (the way back) isn't checked", not err, text)
    await outside("iphone_launch_app", {"app": "时钟"}, True)
    check(c, "Clock joined the scope", "com.apple.mobiletimer" in (await info(lease.id)).get("apps", []))
    n = asked()
    err, text, _ = await call(agent, "iphone_tap_element", {"text": "不存在的按钮xyz"})
    check(c, "acting in Clock no longer asks (and taps nothing)", asked() == n, text)
    await outside("iphone_open_url", {"url": "ictl-device-test://nothing"}, False)
    await outside("iphone_open_url", {"url": "https://example.com"}, False)
    err, text, _ = await call(agent, "iphone_press_button", {"button": "home"})
    await lease.finish()
    check(c, "the task completes (refusals don't make it rejected)", await state_is(lease.id, "completed"))


async def restart(stack, cmd: str):
    """#108: the daemon restarting interrupts the task; its old token means nothing afterwards"""
    c = "restart"
    lease = await Lease.open("真机回归：守护进程重启")
    agent = await mcp(stack, lease.token)
    err, text, _ = await call(agent, "iphone_press_button", {"button": "home"})
    check(c, "the agent acts", not err, text)
    p = await asyncio.create_subprocess_shell(cmd)
    check(c, f"restart command `{cmd}`", await p.wait() == 0)

    async def answers():
        try:
            return await info(lease.id)
        except OSError:
            return None

    t = await wait_for(answers, 120, 1)
    check(c, "after the restart the task is interrupted", (t or {}).get("state") == "interrupted", json.dumps(t, ensure_ascii=False))
    err, text, _ = await call(agent, "iphone_screenshot", timeout=180)
    check(c, "the old token is refused", err, text)
    lease.close()


SCENARIOS = {
    "others": others,
    "pause": pause,
    "handback": handback,
    "confirm": confirm,
    "stop": stop,
    "lease": lease,
    "run": run_cmd,
    "scope": scope,
}


async def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("only", nargs="*", help=f"scenarios ({', '.join([*SCENARIOS, 'restart'])}); all but restart if none")
    ap.add_argument("--restart", metavar="CMD", help="shell command that restarts the daemon, for `restart`")
    a = ap.parse_args()
    names = a.only or list(SCENARIOS)
    unknown = [n for n in names if n not in SCENARIOS and n != "restart"]
    if unknown or ("restart" in names and not a.restart):
        ap.error(f"unknown {unknown}" if unknown else "restart needs --restart CMD")
    print(f"=== {shlex.join(BIN)} ({SOCKET}), MCP: {shlex.join(MCP_CMD)}", flush=True)
    t0 = time.time()
    for name in names:
        busy = [t for t in await tasks() if t["state"] in ("running", "paused", "takeover", "waiting")]
        if busy:
            print(f"FAIL [{name}] another task is active ({busy[0]['id']}, {busy[0]['title']}); not run", flush=True)
            results.append(False)
            continue
        print(f"--- {name}", flush=True)
        t = time.time()
        async with AsyncExitStack() as stack:
            try:
                await (restart(stack, a.restart) if name == "restart" else SCENARIOS[name](stack))
            except Exception as e:  # noqa: BLE001 - a scenario that blew up is a failure, the next one still runs
                check(name, f"scenario raised {type(e).__name__}", False, str(e))
            finally:
                # never leave a task holding the phone
                for t_ in await tasks():
                    if t_["state"] in ("running", "paused", "takeover", "waiting") and t_["owner"] == OWNER:
                        await ask({"op": "task_control", "id": t_["id"], "action": "stop"})
        print(f"    ({time.time() - t:.1f}s)", flush=True)
    failed = results.count(False)
    print(f"--- {len(results)} checks, {failed} failed, {time.time() - t0:.1f}s in all", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
