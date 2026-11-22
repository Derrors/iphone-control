"""Real-device regression runner: start the MCP server over stdio, call tools in order, check each answer, print
it with its wall time, and save screenshots.

    pip install -r tests/device/requirements.txt
    python tests/device/run.py                      # default steps (harmless: Home, Clock tabs, Spotlight search)
    python tests/device/run.py '[["iphone_status", {}], ["sleep", 1], ["iphone_screenshot", {}]]'
    python tests/device/run.py --repeat 5 --json new.json     # timings to compare: see compare.py

The MCP server command comes from MCP_CMD (default: `iphone-control mcp`; for a build of your branch use its
full path, e.g. `target/release/iphone-control mcp`). With MCP_URL set it connects there over HTTP instead (the
viewer's `/mcp`; MCP_CA trusts its self-signed certificate), see mcp_http.py. Screenshots go to SHOTS_DIR (default: ./shots).

It first asks which device it controls (model, iOS version, screen, native or WDA: the `###` line, which a pull
request needs) and at the end checks that it is still the same one: another session switching devices in between
makes the run worthless.

The default steps check what they get, not only that no tool failed: the tab they tap becomes the selected one, the
screen is reported as changed, a screenshot has the screen's size, the text typed into Spotlight shows up. They
work on any screen size and in any language of the phone or of the tools' answers (device.py).

Your own steps are a JSON list. A step is `[tool, arguments]`, or `[tool, arguments, expect]` to check its answer
(`has`, `lacks`, `error`, `max_s`, `image`: see `judge` in device.py); without `expect` a step only has to succeed.
`["sleep", seconds]` waits without calling a tool.

Exit code is 1 if any step failed. The last line sums it up (calls, failures, total time): paste the output into
the pull request, see CONTRIBUTING.md, "Required tests".
"""

import argparse
import asyncio
import base64
import json
import os
import shlex
import sys
import time
from contextlib import AsyncExitStack, asynccontextmanager

from mcp import ClientSession
from mcp.client.stdio import StdioServerParameters, stdio_client

import device as dev
import mcp_http


@asynccontextmanager
async def connect(params: StdioServerParameters):
    """The MCP session: over stdio (MCP_CMD), or at MCP_URL over HTTP (with the task's token, if any)"""
    async with AsyncExitStack() as stack:
        if mcp_http.URL:
            r, w = await mcp_http.streams(stack, os.environ.get("IPHONE_CONTROL_TASK_TOKEN"))
        else:
            r, w = await stack.enter_async_context(stdio_client(params))
        yield await stack.enter_async_context(ClientSession(r, w))


class Runner:
    """Calls tools, prints each answer and keeps what a comparison needs"""

    def __init__(self, session: ClientSession, shots: str):
        self.session = session
        self.shots = shots
        self.calls = self.failed = self.images = 0
        self.records: list[dict] = []
        self.quiet = False
        self.sizes: list[tuple[int, int] | None] = []  # the pictures in the last answer
        self.last_status = ""  # 保留准备阶段的原始状态，避免把未取得尺寸的默认值误认为设备返回了 0×0。

    def fail(self, name: str, why: str):
        """A check the step's own expectations don't cover"""
        self.failed += 1
        if self.records:
            self.records[-1]["ok"] = False
        print(f"  FAILED: {name}: {why}", flush=True)

    async def step(self, name: str, args: dict | None = None, **expect) -> str:
        """Calls the tool and checks its answer against `expect` (device.judge); returns the answer's text"""
        args = args or {}
        t = time.time()
        res = await self.session.call_tool(name, args)
        dt = time.time() - t
        out, sizes = [], []
        for c in res.content:
            if c.type == "image":
                data = base64.b64decode(c.data)
                sizes.append(dev.jpeg_size(data))
                if self.quiet:
                    continue
                self.images += 1
                path = os.path.join(self.shots, f"shot{self.images}.jpg")
                with open(path, "wb") as f:
                    f.write(data)
                out.append(f"<image {path} {sizes[-1]}>")
            else:
                out.append(c.text)
        text = "\n".join(o for o in out if not o.startswith("<image "))
        self.sizes = sizes
        wrong = dev.judge(expect, text, bool(res.is_error), dt, sizes)
        if self.quiet:
            if wrong:
                raise RuntimeError(f"{name}: {'; '.join(wrong)}: {text}")
            return text
        self.calls += 1
        self.failed += bool(wrong)
        self.records.append({"tool": name, "args": args, "seconds": round(dt, 3),
                             "unlock_seconds": dev.unlock_seconds(text), "ok": not wrong})
        flag = "".join(f"\n  FAILED: {w}" for w in wrong) if wrong else ""
        head = f"=== {name} {json.dumps(args, ensure_ascii=False)} {dt:.2f}s{' ERROR' if res.is_error else ''}"
        print(head + "\n" + "\n".join(out) + flag, flush=True)
        if dev.manual_unlock_required(text, bool(res.is_error)):
            if not wrong:
                self.fail(name, "人工解锁阻塞不能作为预期拒绝的通过结果")
            raise dev.ManualUnlockRequired(text)
        return text

    async def look(self, with_apps: bool = True) -> dev.Device:
        """Which device, without printing or counting it as steps. None of these unlock or touch the phone"""
        self.quiet = True
        try:
            d = dev.Device()
            status = await self.step("iphone_status")
            self.last_status = status
            d.model, d.ios, d.udid = dev.controlled(await self.step("iphone_list_devices"), status)
            dev.read_status(d, status)
            if with_apps:
                dev.read_apps(d, await self.step("iphone_list_apps", {"include_system": True}))
            return d
        finally:
            self.quiet = False


async def default_steps(r: Runner, d: dev.Device):
    """Harmless by design: no messages, purchases, deletions or settings changes. Every step says what it expects"""
    cx = round(d.w / 2)
    await r.step("iphone_status")
    # must say "not found" and tap nothing
    await r.step("iphone_tap_element", {"text": "不存在的按钮xyz"}, has=[dev.NOT_FOUND])
    await r.step("iphone_press_button", {"button": "home"})
    await r.step("iphone_launch_app", {"app": d.name(dev.CLOCK)}, has=[dev.CLOCK])
    # enough to reach the tab bar on whichever tab Clock was left (the timer's page has the most before it)
    listed = await r.step("iphone_list_elements", {"limit": 40})
    tabs = dev.tabs(listed)
    if len(tabs) < 2:
        r.fail("iphone_list_elements", "Clock's tab bar isn't among the first 40 elements")
    else:
        # a tab that isn't the selected one: tapping it must make it the selected one. The answer may not say
        # whether the screen changed (a page judged by its picture has no tree to compare with the one before),
        # but it must not say it stayed the same
        was = next(t for t in tabs if t.value == "1")
        other = next(t for t in tabs if t.value != "1")
        for tab in (other, was):  # and back, so that the next run starts from the same screen
            await r.step(
                "iphone_tap_element", {"text": tab.label, "then": "elements", "then_filter": "Button"},
                has=[f'Button "{tab.label}" value="1"'], lacks=[dev.NOT_FOUND, dev.UNCHANGED],
            )
    await r.step("iphone_screenshot", image=[d.w, d.h])
    # the top strip, enlarged: the picture keeps the strip's shape
    await r.step("iphone_screenshot", {"x": 0, "y": 0, "width": d.w, "height": 60})
    got = r.sizes[0] if r.sizes else None
    if not got or abs(got[0] / got[1] - d.w / 60) > 0.03 * d.w / 60 or got[0] < d.w:
        r.fail("iphone_screenshot", f"the strip's picture is {got}: not an enlarged {d.w:g}x60")
    await r.step("iphone_press_button", {"button": "home"})
    # down from the middle of the home screen: Spotlight, with its keyboard up
    await r.step("iphone_swipe", {"from_x": cx, "from_y": round(d.h * 0.43), "to_x": cx, "to_y": round(d.h * 0.75)})
    word = d.name(dev.SETTINGS) if dev.SETTINGS in d.apps else "设置"
    await r.step("iphone_type_text", {"text": word}, lacks=[dev.NO_KEYBOARD])
    await r.step("iphone_wait_for", {"text": word, "timeout": 5}, has=[dev.APPEARED])
    await r.step("iphone_screenshot", image=[d.w, d.h])
    await r.step("iphone_press_button", {"button": "home"})
    await r.step("iphone_status", has=[dev.SPRINGBOARD])


async def main() -> int:
    ap = argparse.ArgumentParser(description="Real-device regression runner (see the top of this file)")
    ap.add_argument("steps", nargs="?", help="steps as a JSON list; the default steps if left out")
    ap.add_argument("--repeat", type=int, default=1, metavar="N", help="run the steps N times (timings: the median counts)")
    ap.add_argument("--json", metavar="FILE", help="write the device and every step's time to FILE, for compare.py")
    a = ap.parse_args()
    steps = json.loads(a.steps) if a.steps else None
    cmd = shlex.split(os.environ.get("MCP_CMD", "iphone-control mcp"))
    shots = os.environ.get("SHOTS_DIR", "shots")
    os.makedirs(shots, exist_ok=True)
    # the whole environment: the MCP SDK passes only a few variables by default, and without XDG_RUNTIME_DIR the
    # server looks for the daemon's socket in the wrong place (every call fails at once)
    params = StdioServerParameters(command=cmd[0], args=cmd[1:], env=dict(os.environ))
    t0 = time.time()
    runs = []
    blocked = False
    async with connect(params) as session:
        await session.initialize()
        r = Runner(session, shots)
        try:
            d = await r.look()
        except Exception as e:  # noqa: BLE001 - whatever went wrong, there is no device to test
            print(f"--- no device to test: {e}", flush=True)
            return 1
        print(f"### {d.line()}", flush=True)
        if not steps and not (d.w and d.h):
            print("--- the screen's size is unknown (is the daemon ready? `iphone-control status`)", flush=True)
            print("--- 准备阶段的原始 iphone_status 响应（0×0 摘要也可能仅表示未取得尺寸字段）：\n" + r.last_status, flush=True)
            return 1
        try:
            for i in range(a.repeat):
                if a.repeat > 1:
                    print(f"### run {i + 1} of {a.repeat}", flush=True)
                r.records = []
                if steps:
                    for name, args, *expect in steps:
                        if name == "sleep":
                            await asyncio.sleep(float(args))
                        else:
                            await r.step(name, args, **(expect[0] if expect else {}))
                else:
                    await default_steps(r, d)
                runs.append(r.records)
        except dev.ManualUnlockRequired:
            blocked = True
            runs.append(r.records)  # 保留失败的部分轮次，不再发送后续操作或下一轮。
            print("ABORT: manual unlock required；剩余步骤未执行，需要在设备上人工解锁", flush=True)
        if not blocked:
            now = await r.look(with_apps=False)
            if now.udid != d.udid:
                r.failed += 1
                print(f"FAILED: the device changed during the run: it was {d.udid}, now {now.udid or 'none'}", flush=True)
    if a.json:
        with open(a.json, "w", encoding="utf-8") as f:
            json.dump({"device": d.line(), "command": shlex.join(cmd), "runs": runs}, f, ensure_ascii=False, indent=1)
    print(f"--- {r.calls} calls, {r.failed} failed, {time.time() - t0:.1f}s in all", flush=True)
    return 1 if r.failed else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
