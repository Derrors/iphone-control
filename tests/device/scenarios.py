"""Real-device scenarios for the tools themselves: the device cases of docs/dev/regression-cases.md that can be
repeated by a script, each check a PASS / FAIL line (tasks.py does the same for AI tasks).

    pip install -r tests/device/requirements.txt
    python tests/device/scenarios.py                  # every scenario except `lock` and `url`
    python tests/device/scenarios.py typing batch     # only these
    python tests/device/scenarios.py --lock lock      # locks the phone and lets it unlock itself: read below first
    python tests/device/scenarios.py --url url        # opens https://example.com in Safari (leaves a tab there)
    python tests/device/scenarios.py --lock --url     # everything

Scenarios (the numbers are regression cases):

  tools    the tools run.py's default steps don't use: tap by coordinates, terminate, list apps and devices,
           selecting the device already selected, an enlarged region (#16), then=screenshot
  typing   typing with no keyboard up is refused and types nothing (#32); the same text typed twice into
           Spotlight counts as typed the second time too (#143)
  delivery one Clock tab tap reports delivery and returns its selected state; a missing target leaves it selected
  collect  find a Clock tab without tapping it, then collect Settings rows from two different viewports
  editing  replace and empty focused text, natively (#159) and through WDA (#162), read element-limit metadata (#160),
           and wait for a visible Clock tab to disappear (#161)
  fresh    a tap right after a cold start isn't swallowed (#13); then=elements shows the page after the tap,
           not the one before (#125)
  batch    iphone_batch runs its steps in order and answers for each; a tap that found nothing stops it and
           the rest is not done
  menu     a long press on a home-screen icon takes about as long as it was asked to (#34); Home closes the menu
           and an app started right afterwards stays in front (#130)
  idle     after the daemon has closed the idle HID session, Home on the home screen and an app started right
           afterwards stays in front (#157); takes about 2 minutes (it waits for the session to close)
  url      a link opened right after its app was terminated opens (#131; in Safari natively, in the default browser
           through WDA); text on the web page can be waited for (#133)
  lock     the lock button locks within seconds, again and again (#59, #102, #105); the next action unlocks first
           and says so (#144); a tap right after unlocking isn't swallowed (#127)

`lock` types the saved passcode into the phone. If the passcode saved for this device is not this device's
(`iphone-control devices` says whether one is saved; `iphone-control passcode set` saves it), the phone counts a
wrong attempt and automatic unlocking is switched off. Make sure of it, then pass --lock.

Everything else is harmless by design: Home, Clock, Spotlight, scrolling Settings, screenshots. Nothing is sent, bought, deleted or
changed. Environment: MCP_CMD, MCP_URL, SHOTS_DIR as for run.py. Exit code 1 if any check failed. Paste the output
into the pull request (CONTRIBUTING.md, "Required tests").
"""

import argparse
import asyncio
import base64
import json
import re
import os
import shlex
import sys
import time
import traceback

from mcp.client.stdio import StdioServerParameters

import device as dev
from run import Runner, connect

results: list[bool] = []
MISSING = "不存在的按钮xyz"
# seconds without a touch before the daemon closes the HID session (crates/ictl-daemon/src/supervise.rs HID_IDLE)
HID_IDLE = 30
AUTO_UNLOCK = dev.T("操作时会自动解锁", "actions unlock automatically")
PINNED = dev.T("(选定)", "(selected)")


def short(text: str, n: int = 170) -> str:
    one = " ".join(text.split())
    return one if len(one) <= n else one[: n - 1] + "…"


def check(case: str, what: str, ok: bool, got: str = "") -> bool:
    # "the phone can't be controlled right now" answers every call with an error: never what a check wanted,
    # also not one that expects a refusal
    ok = bool(ok) and not says(got, dev.UNAVAILABLE)
    results.append(bool(ok))
    print(f"{'PASS' if ok else 'FAIL'} [{case}] {what}" + (f"  ← {short(got)}" if got else ""), flush=True)
    return bool(ok)


def says(text: str, want) -> bool:
    return any(w in text for w in ([want] if isinstance(want, str) else want))


class Phone:
    """The MCP session, with answers as (is_error, text, seconds, picture sizes)"""

    def __init__(self, session, d: dev.Device):
        self.s = session
        self.d = d

    async def result(self, name: str, args: dict | None = None, timeout: float = 90):
        res = await asyncio.wait_for(self.s.call_tool(name, args or {}), timeout)
        text = "\n".join(c.text for c in res.content if c.type == "text")
        if dev.manual_unlock_required(text, bool(res.is_error)):
            raise dev.ManualUnlockRequired(text)
        return res, text

    async def call(self, name: str, args: dict | None = None, timeout: float = 90):
        t = time.time()
        res, text = await self.result(name, args, timeout)
        dt = time.time() - t
        sizes = [dev.jpeg_size(base64.b64decode(c.data)) for c in res.content if c.type == "image"]
        return bool(res.is_error), text, dt, sizes

    async def ok(self, case: str, name: str, args: dict | None = None) -> str:
        """A step the scenario only needs to have worked (getting somewhere); a failure is a failed check"""
        err, text, dt, _ = await self.call(name, args)
        if err:
            check(case, f"{name} {json.dumps(args or {}, ensure_ascii=False)} works", False, text)
        elif took := dev.unlock_seconds(text):
            # worth knowing when reading the times and whatever fails next
            print(f"NOTE [{case}] {name} unlocked the phone first ({took:.1f}s of {dt:.1f}s)", flush=True)
        return text

    async def home(self, case: str):
        await self.ok(case, "iphone_press_button", {"button": "home"})

    async def front(self) -> str:
        return (await self.call("iphone_status"))[1]

    async def clock_tabs(self, case: str) -> list[dev.Element]:
        """Clock's tab bar, as it is now (Clock in front)"""
        _, text, _, _ = await self.call("iphone_list_elements")
        tabs = dev.tabs(text)
        if not tabs:
            front = next((ln for ln in (await self.front()).splitlines() if dev.SPRINGBOARD in ln or "com." in ln), "")
            check(case, "Clock shows its tab bar", False, f"[{front}] {text}")
        return tabs

    async def spotlight(self, case: str):
        d = self.d
        await self.home(case)
        cx = round(d.w / 2)
        await self.ok(case, "iphone_swipe", {"from_x": cx, "from_y": round(d.h * 0.43), "to_x": cx, "to_y": round(d.h * 0.75)})


def selected(text: str, label: str) -> bool:
    return f'Button "{label}" value="1"' in text


# ---------------------------------------------------------------- scenarios


async def tools(p: Phone):
    c, d = "tools", p.d
    await p.home(c)
    await p.ok(c, "iphone_launch_app", {"app": dev.CLOCK})
    tabs = await p.clock_tabs(c)
    if tabs:
        other = next(t for t in tabs if t.value != "1")
        err, text, dt, _ = await p.call("iphone_tap", {"x": other.x, "y": other.y, "then": "elements", "then_filter": "Button"})
        check(c, f"iphone_tap at a tab's coordinates selects that tab ({dt:.2f}s)", not err and selected(text, other.label), text)
        was = next(t for t in tabs if t.value == "1")
        err, text, dt, sizes = await p.call("iphone_tap", {"x": was.x, "y": was.y, "then": "screenshot"})
        check(c, f"then=screenshot returns the screen's picture ({dt:.2f}s)", not err and sizes[:1] == [(round(d.w), round(d.h))], f"{sizes} {text}")
    # 区域输出保留原始像素，只有长边不足下限才额外放大；iPad 的 2 倍原图无需再次放大。
    # 检查是否确实大于点数尺寸且保持长宽比，而不是要求所有设备都超过 2 倍。
    rw, rh = round(d.w / 3), 40
    err, text, dt, sizes = await p.call("iphone_screenshot", {"x": 7, "y": 55, "width": rw, "height": rh})
    got = sizes[0] if sizes else None
    check(c, f"a region of {rw}x{rh} comes back enlarged and in shape ({dt:.2f}s)",
          not err and dev.enlarged_region(got, rw, rh), f"{got} {text}")
    err, text, _, _ = await p.call("iphone_screenshot", {"x": 10, "y": 10})
    check(c, "a region without width and height is refused", err, text)
    err, text, dt, _ = await p.call("iphone_terminate_app", {"app": dev.CLOCK})
    front = await p.front()
    check(c, f"iphone_terminate_app ends Clock: the home screen is in front ({dt:.2f}s)", not err and dev.CLOCK in text and dev.SPRINGBOARD in front, f"{text} / {front}")
    err, text, dt, _ = await p.call("iphone_list_apps", {"include_system": True})
    check(c, f"iphone_list_apps with system apps lists Settings and Clock ({dt:.2f}s)", not err and dev.SETTINGS in text and dev.CLOCK in text, text)
    err, text, _, _ = await p.call("iphone_list_apps")
    check(c, "without them it doesn't list Settings", not err and dev.SETTINGS not in text, text)
    err, text, _, _ = await p.call("iphone_launch_app", {"app": "没有这个App-xyz"})
    check(c, "launching an app that doesn't exist is refused", err, text)
    err, text, _, _ = await p.call("iphone_list_devices")
    check(c, "iphone_list_devices names the device being controlled", not err and d.udid in text, text)
    if says(await p.front(), PINNED) or text.count("iOS") == 1:
        # the device already selected (or the only one): nothing changes hands
        err, text, dt, _ = await p.call("iphone_select_device", {"device": d.udid})
        now = dev.controlled((await p.call("iphone_list_devices"))[1])
        check(c, f"selecting the device already controlled keeps it ({dt:.2f}s)", not err and now[2] == d.udid, text)
    else:
        print(f"SKIP [{c}] iphone_select_device: several devices and none selected (selecting would pin one)", flush=True)
    err, text, _, _ = await p.call("iphone_select_device", {"device": "没有这台设备-xyz"})
    check(c, "selecting a device that isn't there is refused", err, text)


async def typing(p: Phone):
    c, d = "typing", p.d
    await p.home(c)
    err, text, dt, _ = await p.call("iphone_type_text", {"text": "x"})
    check(c, f"#32 typing on the home screen (no keyboard) is refused ({dt:.2f}s)", err and says(text, dev.NO_KEYBOARD), text)
    word = d.name(dev.CLOCK)
    for nth in ("first", "second"):
        await p.spotlight(c)
        err, text, dt, _ = await p.call("iphone_type_text", {"text": word})
        why = "" if nth == "first" else " (#143: Spotlight still holds it, selected)"
        check(c, f"typing {word!r} into Spotlight the {nth} time counts as typed{why} ({dt:.2f}s)", not err, text)
        err, text, dt, _ = await p.call("iphone_wait_for", {"text": word, "timeout": 5})
        check(c, f"…and it is on screen ({dt:.2f}s)", not err and says(text, dev.APPEARED), text)
    err, text, dt, _ = await p.call("iphone_wait_for", {"text": MISSING, "timeout": 2})
    check(c, f"waiting for text that never comes gives up after its timeout ({dt:.2f}s)", 1.5 < dt < 6 and not says(text, dev.APPEARED), text)
    await p.home(c)


def spotlight_queries(text: str) -> list[str]:
    # describe() prints a field value only once when it equals the label.
    return [e.value if e.value is not None else e.label for e in dev.elements(text)
            if e.kind in ("SearchField", "TextField")]


async def editing(p: Phone):
    c = "editing"
    try:
        await p.spotlight(c)
        await p.ok(c, "iphone_type_text", {"text": "wifi"})
        before = await p.ok(c, "iphone_list_elements", {"filter": "SearchField"})
        if not check("#159", "Spotlight query is wifi before replacement",
                     spotlight_queries(before) == ["wifi"], before):
            return
        for wanted in ("wi", "蓝牙"):
            res, answer = await p.result("iphone_type_text", {"text": wanted, "replace": True})
            data = res.structured_content or {}
            readback = await p.ok(c, "iphone_list_elements", {"filter": "SearchField"})
            if not check("#159", f"replace the focused Spotlight query with {wanted!r} and verify its field",
                         not res.is_error and data.get("delivery") == "sent" and data.get("readback", {}).get("field") == "verified"
                         and spotlight_queries(readback) == [wanted], f"delivery={data.get('delivery')} readback={data.get('readback')} {answer}"):
                return
        # empty text empties it; the empty field reads its placeholder as its value
        err, answer, dt, _ = await p.call("iphone_type_text", {"text": "", "replace": True})
        readback = await p.ok(c, "iphone_list_elements", {"filter": "SearchField"})
        check("#159", f"replace with empty text empties the Spotlight query ({dt:.2f}s)",
              not err and says(answer, dev.T("已清空", "Emptied")) and "蓝牙" not in readback,
              answer if err else readback)
    finally:
        await p.home(c)
    err, answer, _, _ = await p.call("iphone_type_text", {"text": "蓝牙", "replace": True, "submit": True})
    check("#159", "replacement without a keyboard is refused", err and says(answer, dev.NO_KEYBOARD), answer)

    await p.ok(c, "iphone_launch_app", {"app": dev.CLOCK})
    tabs = await p.clock_tabs(c)
    if not tabs:
        return
    for limit in (1, 500):
        res = await asyncio.wait_for(p.s.call_tool("iphone_list_elements", {"limit": limit}), 30)
        data = res.structured_content or {}
        count = len(data.get("elements", []))
        matched = data.get("matched_count")
        expected = limit == 1
        check("#160", f"element limit {limit} reports truncation",
              not res.is_error and isinstance(matched, int) and matched > 1 and count == min(matched, limit)
              and data.get("truncated") is expected,
              f"returned={count} matched={matched} truncated={data.get('truncated')}")

    target = {"text": tabs[0].label, "state": "disappeared", "timeout": 2}
    err, answer, dt, _ = await p.call("iphone_wait_for", target)
    check("#161", f"still-visible Clock tab times out ({dt:.2f}s)",
          err and says(answer, dev.T("没消失", "did not disappear")), answer)
    await p.home(c)
    err, answer, dt, _ = await p.call("iphone_wait_for", target)
    check("#161", f"Clock tab is gone after leaving the app ({dt:.2f}s)",
          not err and says(answer, dev.T("已消失", "Disappeared")), answer)


async def delivery(p: Phone):
    c = "delivery"
    await p.ok(c, "iphone_launch_app", {"app": dev.CLOCK})
    tabs = await p.clock_tabs(c)
    if not tabs:
        return
    was = next(t for t in tabs if t.value == "1")
    target = next(t for t in tabs if t.value != "1")
    try:
        res, text = await p.result("iphone_tap", {"x": target.x, "y": target.y, "then": "elements", "then_filter": "Button"})
        data = res.structured_content or {}
        check(c, "one tap reports sent, reads the screen and selects the requested Clock tab",
              not res.is_error and data.get("delivery") == "sent" and data.get("readback", {}).get("screen") == "ok"
              and any(e.get("label") == target.label for e in data.get("elements", []))
              and selected(text, target.label), f"delivery={data.get('delivery')} readback={data.get('readback')} {text}")

        res, text = await p.result("iphone_tap_element", {"text": MISSING})
        data = res.structured_content or {}
        now = await p.clock_tabs(c)
        check(c, "a missing tap target reports not_sent and leaves the tab selected",
              not res.is_error and says(text, dev.NOT_FOUND) and data.get("delivery") == "not_sent"
              and any(t.label == target.label and t.value == "1" for t in now), f"delivery={data.get('delivery')} {text}")
    finally:
        await p.ok(c, "iphone_tap_element", {"text": was.label})
        await p.home(c)


def list_rows(items: list[dict]) -> set[tuple[str, str]]:
    # Across native and WDA a Settings row can be a button, cell or its text child.
    return {(e["type"], e["label"]) for e in items
            if e.get("type") in ("Button", "Cell", "StaticText") and e.get("label")}


async def collect(p: Phone):
    c = "collect"
    try:
        await p.ok(c, "iphone_launch_app", {"app": dev.CLOCK})
        tabs = await p.clock_tabs(c)
        if not tabs:
            return
        was = next(t for t in tabs if t.value == "1")
        target = next(t for t in tabs if t.value != "1")
        res, text = await p.result("iphone_scroll_collect", {"until": target.label})
        data = res.structured_content or {}
        check(c, "a visible Clock tab is found on the first screen",
              not res.is_error and data.get("stop_reason") == "found" and data.get("scrolls") == 0
              and (data.get("matched") or {}).get("label") == target.label, text)
        now = await p.clock_tabs(c)
        check(c, "finding the tab leaves the original tab selected",
              any(t.label == was.label and t.value == "1" for t in now))

        await p.ok(c, "iphone_launch_app", {"app": dev.SETTINGS})
        for direction in ("down", "up"):
            before, _ = await p.result("iphone_list_elements", {"limit": 500})
            res, text = await p.result("iphone_scroll_collect", {"scroll": direction, "until": MISSING, "max_scrolls": 1})
            after, _ = await p.result("iphone_list_elements", {"limit": 500})
            data = res.structured_content or {}
            if not check(c, f"one scroll {direction} stops at the bound or the list end with no match",
                         not res.is_error and data.get("stop_reason") in ("max_scrolls", "end")
                         and data.get("scrolls") == 1 and data.get("matched") is None, text.splitlines()[0] if text else ""):
                return
            start = before.structured_content or {}
            end = after.structured_content or {}
            if not check(c, "both independent viewport reads are complete",
                         not before.is_error and not after.is_error and start.get("truncated") is False
                         and end.get("truncated") is False):
                return
            first, last = list_rows(start["elements"]), list_rows(end["elements"])
            collected = list_rows(data["rows"])
            old, new = (first - last) & collected, (last - first) & collected
            if first - last and last - first:
                check(c, "the collection includes rows unique to each independently observed viewport",
                      bool(old and new), f"{len(old)} first-screen-only, {len(new)} final-screen-only")
                return
            # Settings may reopen at the bottom. Try the opposite direction once.
        check(c, "one collection includes rows unique to each independently observed viewport", False)
    finally:
        await p.home(c)


async def fresh(p: Phone):
    c = "fresh"
    await p.home(c)
    await p.ok(c, "iphone_launch_app", {"app": dev.CLOCK})
    tabs = await p.clock_tabs(c)
    if not tabs:
        return
    for n in range(3):
        await p.ok(c, "iphone_terminate_app", {"app": dev.CLOCK})
        await p.ok(c, "iphone_launch_app", {"app": dev.CLOCK})
        err, before, _, _ = await p.call("iphone_list_elements")
        # 冷启动后各系统恢复的标签不同，每次从实际页面选一个尚未选中的标签，不能靠固定轮换。
        current = dev.tabs(before)
        if not check(c, "冷启动后读到时钟标签栏", not err and bool(current), before):
            return
        target = next(t for t in reversed(current) if t.value != "1")
        err, text, dt, _ = await p.call("iphone_tap_element", {"text": target.label, "then": "elements"})
        after = text.split("---")[-1]
        check(c, f"#13 a tap right after a cold start takes effect, {n + 1} of 3 ({dt:.2f}s)",
              not err and selected(after, target.label) and not says(text, dev.UNCHANGED), text)
        old = {e.label for e in dev.elements(before)} - {t.label for t in current}
        new = {e.label for e in dev.elements(after)} - {t.label for t in current}
        check(c, "#125 then=elements shows the new page, not the one before the tap", bool(new) and new != old and not says(text, dev.UNCHANGED),
              f"before {sorted(old)[:4]}, after {sorted(new)[:4]}")
    await p.home(c)


async def idle(p: Phone):
    c = "idle"
    for n in range(2):
        await p.home(c)
        await asyncio.sleep(2)
        # the daemon closes the HID session after 30 s without a touch (supervise::HID_IDLE); not with the control
        # page showing the screen, when this passes either way
        await asyncio.sleep(HID_IDLE + 5)
        await p.home(c)
        await p.ok(c, "iphone_launch_app", {"app": dev.CLOCK})
        await asyncio.sleep(1.5)
        status = await p.front()
        check(c, f"#157 Home with the HID session idle, then an app started: it stays in front, {n + 1} of 2",
              dev.CLOCK in status, status)
    await p.home(c)


async def batch(p: Phone):
    c = "batch"
    await p.home(c)
    steps = [
        {"tool": "iphone_launch_app", "app": dev.CLOCK},
        {"tool": "wait", "seconds": 0.5},
        {"tool": "iphone_wait_for", "text": "", "timeout": 3},
        {"tool": "iphone_press_button", "button": "home"},
    ]
    await p.ok(c, "iphone_launch_app", {"app": dev.CLOCK})
    tabs = await p.clock_tabs(c)
    await p.home(c)
    if not tabs:
        return
    steps[2]["text"] = tabs[0].label
    err, text, dt, _ = await p.call("iphone_batch", {"steps": steps, "then": "elements"})
    lines = [ln for ln in text.splitlines() if ln[:2] in ("1.", "2.", "3.", "4.")]
    check(c, f"a batch of 4 steps answers for each of them, in order ({dt:.2f}s)", not err and len(lines) == 4 and dev.CLOCK in lines[0], text)
    check(c, "its then=elements shows the screen after the last step (the home screen)", "Icon " in text.split("---")[-1], text.split("---")[-1])
    check(c, "the home screen is in front", dev.SPRINGBOARD in await p.front())
    # a tap that found nothing stops the batch: Clock is started, but Home (the step after) is not pressed
    steps = [
        {"tool": "iphone_launch_app", "app": dev.CLOCK},
        {"tool": "iphone_tap_element", "text": MISSING},
        {"tool": "iphone_press_button", "button": "home"},
    ]
    err, text, dt, _ = await p.call("iphone_batch", {"steps": steps})
    front = await p.front()
    check(c, f"a tap that found nothing stops the batch with an error ({dt:.2f}s)", err and says(text, dev.NOT_FOUND) and "3." not in text, text)
    check(c, "…and the step after it was not done (Clock is still in front)", dev.CLOCK in front, front)
    for bad, why in (([{"tool": "iphone_screenshot"}], "a tool that isn't an action"), ([{"tool": "iphone_tap", "x": 1}], "a step missing an argument")):
        err, text, _, _ = await p.call("iphone_batch", {"steps": [{"tool": "iphone_press_button", "button": "home"}, *bad]})
        front = await p.front()
        check(c, f"a batch with {why} is refused before anything runs (Clock still in front)", err and dev.CLOCK in front, text)
    await p.home(c)


async def menu(p: Phone):
    c = "menu"
    await p.home(c)
    _, text, _, _ = await p.call("iphone_list_elements")
    icons = [e for e in dev.elements(text) if e.kind == "Icon" and e.h < 130 and e.w < 130]
    if not check(c, "the home screen lists app icons", bool(icons), text):
        return
    icon = icons[0]
    for n in range(3):
        err, text, dt, _ = await p.call("iphone_long_press", {"x": icon.x, "y": icon.y, "seconds": 1.2, "then": "elements"})
        check(c, f"#34 a 1.2 s long press on an icon takes seconds, not ten ({dt:.2f}s incl. settling)", not err and dt < 6, text.splitlines()[0])
        if n == 0:
            check(c, "…and its menu is up (the screen changed)", not says(text, dev.UNCHANGED), short(text, 300))
        await p.home(c)
        err, text, dt, _ = await p.call("iphone_launch_app", {"app": dev.CLOCK})
        await asyncio.sleep(1.5)  # the menu's closing animation used to send the app back about now
        front = await p.front()
        check(c, f"#130 Clock started right after Home closed the menu stays in front, {n + 1} of 3", not err and dev.CLOCK in front, f"{text} / {front}")
        await p.home(c)


async def url(p: Phone):
    c = "url"
    await p.home(c)
    await p.ok(c, "iphone_launch_app", {"app": dev.SAFARI})
    await p.ok(c, "iphone_terminate_app", {"app": dev.SAFARI})
    err, text, dt, _ = await p.call("iphone_open_url", {"url": "https://example.com"})
    # iPad 的过渡动画中回答可能暂时写 DockFolderViewService；用实际前台状态核对启动结果。
    front = await p.front()
    # natively a web link always opens in Safari; through WDA the system opens it in the default browser, which may be
    # another one (Chrome): the app the answer names, in front and not the home screen, is as good
    m = re.search(r"(?:前台|foreground) ([\w.-]+)", text)
    opened = m.group(1) if m else ""
    in_browser = dev.SAFARI in front or (bool(opened) and opened != dev.SPRINGBOARD and opened in front)
    check(c, f"#131 a link opened right after Safari was terminated opens in a browser ({opened or '?'}, {dt:.2f}s)",
          not err and "https://example.com" in text and in_browser, f"{text} / {front}")
    if not err and dev.SAFARI in front and dev.SAFARI not in text:
        print(f"NOTE [{c}] 打开链接的回答和随后查询的前台 App 不一致：{short(text)}", flush=True)
    # a word of the page's English paragraph (the page has no heading in the tree, and its first paragraph follows
    # the phone's language)
    err, text, dt, _ = await p.call("iphone_wait_for", {"text": "documentation", "timeout": 15})
    check(c, f"#133 text on the web page can be waited for ({dt:.2f}s)", not err and says(text, dev.APPEARED), text)
    names = {t.name for t in (await p.s.list_tools()).tools}
    if "iphone_web_targets" in names:
        times = []
        for _ in range(4):
            err, text, dt, _ = await p.call("iphone_web_targets")
            times.append(dt)
        check(c, f"#51 iphone_web_targets finds the page, and fast after the first time ({', '.join(f'{t:.2f}' for t in times)} s)",
              not err and "example" in text.lower() and max(times[1:]) < 3, text)
    else:
        print(f"SKIP [{c}] #51 iphone_web_targets: the developer tools are off (IPHONE_CONTROL_DEV_TOOLS=1)", flush=True)
    err, text, _, _ = await p.call("iphone_open_url", {"url": "not a url"})
    check(c, "something that isn't a link is refused", err, text)
    await p.home(c)


async def lock(p: Phone):
    c = "lock"
    await p.home(c)
    await p.ok(c, "iphone_launch_app", {"app": dev.CLOCK})
    tabs = await p.clock_tabs(c)
    if not tabs:
        return
    times = []
    for n in range(3):
        err, text, dt, _ = await p.call("iphone_press_button", {"button": "lock"})
        # the status may still say unlocked for a moment after the button's answer (seen: about 0.3 s)
        t0, locked, status = time.time(), False, ""
        while not locked and time.time() - t0 < 3:
            status = await p.front()
            locked = dev.is_locked(status)
            if not locked:
                await asyncio.sleep(0.2)
        seen = time.time() - t0
        times.append(dt + seen)
        if not check(c, f"the lock button locks the phone, {n + 1} of 3 ({dt:.2f}s, the status says so {seen:.2f}s later)",
                     not err and locked, f"{text} / {status}"):
            return
        if not check(c, "the status says the next action unlocks it", says(status, AUTO_UNLOCK), status):
            print(f"STOP [{c}] automatic unlocking is not available: unlock the phone by hand", flush=True)
            return
        # right after it locked, the daemon doesn't unlock (3 s): not what is measured here
        await asyncio.sleep(4)
        target = next(t for t in tabs if t.value != "1") if n % 2 == 0 else next(t for t in tabs if t.value == "1")
        err, text, dt, _ = await p.call("iphone_tap_element", {"text": target.label, "then": "elements", "then_filter": "Button"}, timeout=120)
        took = dev.unlock_seconds(text)
        check(c, f"#144 the next action unlocks first and says how long it took ({took:.1f}s of {dt:.1f}s)", not err and 0 < took < 30, text.splitlines()[0])
        check(c, "#127 the tap right after unlocking takes effect", selected(text, target.label), text)
        if err or not took:
            print(f"STOP [{c}] it did not unlock: not locking again", flush=True)
            return
    check(c, f"#102 #105 locking takes seconds each time ({', '.join(f'{t:.1f}' for t in times)} s)", max(times[1:]) < 5, "")
    await p.home(c)


SCENARIOS = {
    "tools": tools, "typing": typing, "editing": editing, "fresh": fresh, "batch": batch,
    "delivery": delivery, "collect": collect, "menu": menu, "idle": idle, "url": url, "lock": lock
}
OPT_IN = {"lock": "--lock", "url": "--url"}


async def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("only", nargs="*", help=f"scenarios ({', '.join(SCENARIOS)}); all but lock and url if none")
    ap.add_argument("--lock", action="store_true", help="allow `lock`: the saved passcode is this device's")
    ap.add_argument("--url", action="store_true", help="allow `url`: opens https://example.com in Safari")
    a = ap.parse_args()
    names = a.only or [n for n in SCENARIOS if n not in OPT_IN or getattr(a, OPT_IN[n][2:])]
    if unknown := [n for n in names if n not in SCENARIOS]:
        ap.error(f"unknown {unknown}")
    for n, flag in OPT_IN.items():
        if n in names and not getattr(a, flag[2:]):
            ap.error(f"`{n}` needs {flag}: see --help")
    cmd = shlex.split(os.environ.get("MCP_CMD", "iphone-control mcp"))
    params = StdioServerParameters(command=cmd[0], args=cmd[1:], env=dict(os.environ))
    t0 = time.time()
    async with connect(params) as session:
        await session.initialize()
        d = await Runner(session, os.environ.get("SHOTS_DIR", "shots")).look()
        print(f"### {d.line()}", flush=True)
        if not dev.is_expected(d):
            return 1
        if not (d.udid and d.w and d.h):
            print("--- no device ready to test (`iphone-control status`)", flush=True)
            return 1
        p = Phone(session, d)
        for name in names:
            print(f"--- {name}", flush=True)
            # WDA restarting (below iOS 27 there is nothing else to fall back on) fails every call for a minute
            # or two: wait for it instead of failing a whole scenario over it
            t = time.time()
            while not dev.ready(await p.front()) and time.time() - t < 180:
                await asyncio.sleep(3)
            if time.time() - t > 3:
                print(f"NOTE [{name}] waited {time.time() - t:.0f}s for the phone to be ready", flush=True)
            if not dev.ready(await p.front()):
                check(name, "the phone is ready", False, await p.front())
                break
            t = time.time()
            try:
                await SCENARIOS[name](p)
            except dev.ManualUnlockRequired as e:
                check(name, "manual unlock required", False, str(e))
                print("ABORT: manual unlock required；剩余场景未执行，需要在设备上人工解锁", flush=True)
                break
            except Exception as e:  # noqa: BLE001 - a scenario that blew up is a failure, the next one still runs
                traceback.print_exc()
                check(name, f"scenario raised {type(e).__name__}", False, str(e))
            now = dev.controlled((await p.call("iphone_list_devices"))[1])[2]
            if now != d.udid:
                check(name, "the same device is still controlled", False, f"it was {d.udid}, now {now or 'none'}")
                break
            print(f"    ({time.time() - t:.1f}s)", flush=True)
    failed = results.count(False)
    print(f"--- {len(results)} checks, {failed} failed, {time.time() - t0:.1f}s in all", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
