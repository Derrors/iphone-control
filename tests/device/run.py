"""Real-device regression runner: start the MCP server over stdio, call tools in order, print each
result with its wall time, and save screenshots.

    pip install -r tests/device/requirements.txt
    python tests/device/run.py                      # default steps (harmless: Home, Clock tabs, Spotlight search)
    python tests/device/run.py '[["iphone_status", {}], ["sleep", 1], ["iphone_screenshot", {}]]'

The MCP server command comes from MCP_CMD (default: `iphone-control mcp`; for a build of your branch use its
full path, e.g. `target/release/iphone-control mcp`). With MCP_URL set it connects there over HTTP instead (the
viewer's `/mcp`; MCP_CA trusts its self-signed certificate), see mcp_http.py. Screenshots go to SHOTS_DIR (default: ./shots).
A step `["sleep", seconds]` waits without calling a tool. Exit code is 1 if any tool returned an error.
The last line sums it up (calls, failures, total time): paste the output into the pull request, see
CONTRIBUTING.md, "Required tests".
"""

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

import mcp_http

# Harmless by design: no messages, purchases, deletions or settings changes.
DEFAULT_STEPS = [
    ["iphone_status", {}],
    ["iphone_tap_element", {"text": "不存在的按钮xyz"}],  # must report "not found", not tap anything
    ["iphone_press_button", {"button": "home"}],
    ["iphone_launch_app", {"app": "时钟"}],
    ["iphone_list_elements", {"limit": 12}],
    ["iphone_tap_element", {"text": "秒表", "then": "elements", "then_filter": "Button"}],
    ["iphone_screenshot", {}],
    ["iphone_screenshot", {"x": 0, "y": 0, "width": 430, "height": 60}],
    ["iphone_press_button", {"button": "home"}],
    ["iphone_swipe", {"from_x": 215, "from_y": 400, "to_x": 215, "to_y": 700}],
    ["iphone_type_text", {"text": "设置"}],
    ["iphone_wait_for", {"text": "设置", "timeout": 5}],
    ["iphone_screenshot", {}],
    ["iphone_press_button", {"button": "home"}],
]


@asynccontextmanager
async def connect(params: StdioServerParameters):
    """The MCP session: over stdio (MCP_CMD), or at MCP_URL over HTTP (with the task's token, if any)"""
    async with AsyncExitStack() as stack:
        if mcp_http.URL:
            r, w = await mcp_http.streams(stack, os.environ.get("IPHONE_CONTROL_TASK_TOKEN"))
        else:
            r, w = await stack.enter_async_context(stdio_client(params))
        yield await stack.enter_async_context(ClientSession(r, w))


async def main() -> int:
    steps = json.loads(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_STEPS
    cmd = shlex.split(os.environ.get("MCP_CMD", "iphone-control mcp"))
    shots = os.environ.get("SHOTS_DIR", "shots")
    os.makedirs(shots, exist_ok=True)
    failed = 0
    # the whole environment: the MCP SDK passes only a few variables by default, and without XDG_RUNTIME_DIR the
    # server looks for the daemon's socket in the wrong place (every call fails at once)
    params = StdioServerParameters(command=cmd[0], args=cmd[1:], env=dict(os.environ))
    calls = 0
    t0 = time.time()
    async with connect(params) as session:
        await session.initialize()
        n = 0
        for name, args in steps:
            if name == "sleep":
                await asyncio.sleep(float(args))
                continue
            t = time.time()
            res = await session.call_tool(name, args)
            dt = time.time() - t
            out = []
            for c in res.content:
                if c.type == "image":
                    n += 1
                    path = os.path.join(shots, f"shot{n}.jpg")
                    with open(path, "wb") as f:
                        f.write(base64.b64decode(c.data))
                    out.append(f"<image {path}>")
                else:
                    out.append(c.text)
            calls += 1
            failed += bool(res.is_error)
            flag = " ERROR" if res.is_error else ""
            print(f"=== {name} {json.dumps(args, ensure_ascii=False)} {dt:.2f}s{flag}\n" + "\n".join(out), flush=True)
    print(f"--- {calls} calls, {failed} failed, {time.time() - t0:.1f}s in all", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
