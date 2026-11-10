"""Real-device regression runner: start the MCP server over stdio, call tools in order, print each
result with its wall time, and save screenshots.

    pip install -r tests/device/requirements.txt
    python tests/device/run.py                      # default steps (harmless: Home, Clock tabs, Spotlight search)
    python tests/device/run.py '[["iphone_status", {}], ["sleep", 1], ["iphone_screenshot", {}]]'

The MCP server command comes from MCP_CMD (default: `iphone-control mcp`;
for a source install use e.g. `iphone-control mcp`). Screenshots go to SHOTS_DIR (default: ./shots).
A step `["sleep", seconds]` waits without calling a tool. Exit code is 1 if any tool returned an error.
"""

import asyncio
import base64
import json
import os
import shlex
import sys
import time

from mcp import ClientSession
from mcp.client.stdio import StdioServerParameters, stdio_client

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


async def main() -> int:
    steps = json.loads(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_STEPS
    cmd = shlex.split(os.environ.get("MCP_CMD", "iphone-control mcp"))
    shots = os.environ.get("SHOTS_DIR", "shots")
    os.makedirs(shots, exist_ok=True)
    failed = 0
    params = StdioServerParameters(command=cmd[0], args=cmd[1:])
    async with stdio_client(params) as (r, w), ClientSession(r, w) as session:
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
            failed += bool(res.is_error)
            flag = " ERROR" if res.is_error else ""
            print(f"=== {name} {json.dumps(args, ensure_ascii=False)} {dt:.2f}s{flag}\n" + "\n".join(out), flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
