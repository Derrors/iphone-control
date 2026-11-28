"""What run.py and scenarios.py share: reading the device out of the tools' answers, checking an answer against
what is expected of it, and a few texts in both languages.

The tools answer in Chinese or English (IPHONE_CONTROL_LANG of the daemon and the MCP server), and the phone has
its own language, so nothing here depends on either: an expected text is given in both languages (`T`), and app
names are read from the phone (`iphone_list_apps`)."""

import os
import re
import struct
from dataclasses import dataclass, field

CLOCK = "com.apple.mobiletimer"
SETTINGS = "com.apple.Preferences"
SAFARI = "com.apple.mobilesafari"
SPRINGBOARD = "com.apple.springboard"


def T(zh: str, en: str) -> list[str]:
    """A text the tools say, in both languages: either one counts"""
    return [zh, en]


NOT_FOUND = T("没找到", "Not found")
CHANGED = T("界面有变化", "the screen changed")
UNCHANGED = T("界面和操作前一样", "the screen is the same")
APPEARED = T("已出现", "Appeared")
NO_KEYBOARD = T("屏幕上没有键盘", "No keyboard on screen")
AFTER = T("操作后的界面", "after the action")
UNAVAILABLE = T("WDA 不可用", "WDA unavailable")  # the phone can't be controlled right now: not an answer to the call
MANUAL_UNLOCK = T("需要人工解锁", "unlock it by hand")


class ManualUnlockRequired(RuntimeError):
    """当前调用已结束且明确要求人工解锁；记录失败后停止后续操作。"""


def manual_unlock_required(text: str, is_error: bool) -> bool:
    return is_error and any(s in text for s in MANUAL_UNLOCK)


def ready(status_text: str) -> bool:
    """Does iphone_status say the phone can be controlled (it then knows the screen's size)?"""
    return bool(_SCREEN.search(status_text)) and not any(u in status_text for u in UNAVAILABLE)

_UNLOCKED = re.compile(r"先自动解锁了,用时 ([\d.]+) 秒|unlocked first, which took ([\d.]+) s")
_DEVICE = re.compile(r"\((iP[A-Za-z]+\d+,\d+) (?:iOS|iPadOS) ([\d.]+), ?([0-9A-Fa-f-]{24,40})\)")
_SCREEN = re.compile(r"(?:屏幕|Screen): (\d+(?:\.\d+)?)x(\d+(?:\.\d+)?)")
_ELEMENT = re.compile(r'^(\w+) "((?:[^"\\]|\\.)*)"(.*?) @\((-?\d+(?:\.\d+)?),(-?\d+(?:\.\d+)?)\) (\d+(?:\.\d+)?)x(\d+(?:\.\d+)?)')


def unlock_seconds(text: str) -> float:
    """How long the answer says the phone took to unlock first (0: it didn't have to)"""
    m = _UNLOCKED.search(text)
    return float(m.group(1) or m.group(2)) if m else 0.0


@dataclass
class Element:
    kind: str
    label: str
    rest: str
    x: float
    y: float
    w: float
    h: float

    @property
    def value(self) -> str | None:
        m = re.search(r'value="([^"]*)"', self.rest)
        return m.group(1) if m else None


def elements(text: str) -> list[Element]:
    """The elements in an answer of iphone_list_elements (or after `--- the screen after the action ---`)"""
    out = []
    for line in text.splitlines():
        if m := _ELEMENT.match(line.strip()):
            out.append(Element(m[1], m[2], m[3], *(float(m[i]) for i in range(4, 8))))
    return out


def tabs(text: str) -> list[Element]:
    """找水平相邻的一组标签按钮，不按文字或顶部/底部位置判断。

    iPad 的工具栏按钮和标签可能在同一行，必须先按间距分组，不能把远处的编辑/添加算作标签。
    每组至少三个按钮且仅一个 value=1；原生未选中时为 0，WDA 则可能不返回 value。
    """
    rows: dict[tuple[float, float], list[Element]] = {}
    for e in elements(text):
        if e.kind == "Button":
            rows.setdefault((e.y, e.h), []).append(e)
    candidates = []
    for row in rows.values():
        groups: list[list[Element]] = []
        right = 0.0
        for e in sorted(row, key=lambda e: e.x):
            if not groups or e.x - e.w / 2 > right + e.h:
                groups.append([e])
                right = e.x + e.w / 2
            else:
                groups[-1].append(e)
                right = max(right, e.x + e.w / 2)
        for group in groups:
            if len(group) >= 3 and sum(e.value == "1" for e in group) == 1 and all(e.value in ("0", "1", None) for e in group):
                candidates.append(group)
    return max(candidates, key=len, default=[])


@dataclass
class Device:
    model: str = "?"
    ios: str = "?"
    udid: str = ""
    w: float = 0
    h: float = 0
    native: bool = False
    locked: bool = False
    apps: dict[str, str] = field(default_factory=dict)  # bundle ID → the name the phone shows

    def line(self) -> str:
        how = "native" if self.native else "WDA"
        return f"{self.model}, iOS {self.ios}, {self.w:g}x{self.h:g} pt, {how}, {self.udid}"

    def name(self, bundle: str) -> str:
        """The app's name on this phone (to launch it the way an agent would), or its bundle ID"""
        return self.apps.get(bundle, bundle)


def controlled(devices_text: str, status_text: str = "") -> tuple[str, str, str]:
    """(model, iOS version, UDID) of the device being controlled, from iphone_list_devices: the line with the arrow
    when several are connected, else the only one"""
    lines = [ln for ln in devices_text.splitlines() if "→" in ln] or devices_text.splitlines() + status_text.splitlines()
    for ln in lines:
        if m := _DEVICE.search(ln):
            return m[1], m[2], m[3]
    return "?", "?", ""


def is_expected(d: Device) -> bool:
    """With EXPECT_UDID set, whether `d` is that device (in either case); says so when it isn't. Two instances
    tested side by side must each be on its own device before doing anything (instances.py)"""
    want = os.environ.get("EXPECT_UDID", "").strip()
    if want and want.upper() != (d.udid or "").upper():
        print(f"--- controlling {d.udid or 'no device'}, not EXPECT_UDID={want}: stopped before any step", flush=True)
        return False
    return True


def read_status(d: Device, text: str) -> Device:
    if m := _SCREEN.search(text):
        d.w, d.h = float(m[1]), float(m[2])
    first = text.splitlines()[0] if text else ""
    d.native = "原生" in first or "native" in first.lower()
    d.locked = is_locked(text)
    return d


def is_locked(status_text: str) -> bool:
    """Does iphone_status say the phone is locked? Its line reads `锁屏: 已锁屏(…)` / `Locked: locked (…)`"""
    line = next((ln for ln in status_text.splitlines() if ln.startswith(("锁屏:", "Locked:"))), "")
    return line.partition(":")[2].strip().startswith(("已锁屏", "locked"))


def read_apps(d: Device, text: str) -> Device:
    for ln in text.splitlines():
        name, tab, bundle = ln.partition("\t")
        if tab and bundle.strip():
            d.apps.setdefault(bundle.strip(), name.strip())
    return d


def jpeg_size(data: bytes) -> tuple[int, int] | None:
    """(width, height) of a JPEG, from its frame header; PNG too. None when it is neither"""
    if data[:8] == b"\x89PNG\r\n\x1a\n":
        return struct.unpack(">II", data[16:24])
    if data[:2] != b"\xff\xd8":
        return None
    i = 2
    while i + 9 < len(data):
        if data[i] != 0xFF:
            i += 1
            continue
        marker = data[i + 1]
        if marker in (0xC0, 0xC1, 0xC2):
            h, w = struct.unpack(">HH", data[i + 5 : i + 9])
            return w, h
        if marker in (0xD8, 0x01) or 0xD0 <= marker <= 0xD7:
            i += 2
            continue
        i += 2 + struct.unpack(">H", data[i + 2 : i + 4])[0]
    return None


def enlarged_region(size: tuple[int, int] | None, width: float, height: float) -> bool:
    """区域图片应比点数尺寸大，且保持长宽比；原始截图恰好为 2 倍也合法。"""
    if size is None or width <= 0 or height <= 0 or size[1] <= 0:
        return False
    ratio = width / height
    return size[0] > width and abs(size[0] / size[1] - ratio) < 0.02 * ratio


def _says(text: str, want) -> bool:
    """`want`: a text, or the same text in several languages (any one of them)"""
    return any(w in text for w in ([want] if isinstance(want, str) else want))


def judge(expect: dict, text: str, is_error: bool, seconds: float, images: list[tuple[int, int] | None]) -> list[str]:
    """What is wrong with an answer, given what the step expects of it (nothing expected: only that it isn't an
    error). `expect`:

      error   true: the tool must refuse (an error result); default false
      has     texts the answer must contain; each a text, or a list of alternatives (the two languages)
      lacks   texts it must not contain, likewise
      max_s   seconds it may take, not counting an automatic unlock the answer reports
      image   [width, height] the returned picture must have, in pixels (±1)
    """
    wrong = []
    if bool(expect.get("error", False)) != is_error:
        wrong.append("was refused" if is_error else "should have been refused")
    for w in expect.get("has", []):
        if not _says(text, w):
            wrong.append(f"doesn't say {w!r}")
    for w in expect.get("lacks", []):
        if _says(text, w):
            wrong.append(f"says {w!r}")
    if "max_s" in expect and seconds - unlock_seconds(text) > expect["max_s"]:
        wrong.append(f"took {seconds - unlock_seconds(text):.2f}s, over {expect['max_s']:g}s")
    if "image" in expect:
        want = tuple(expect["image"])
        got = images[0] if images else None
        if not got or any(abs(g - w) > 1 for g, w in zip(got, want)):
            wrong.append(f"picture is {got}, not {want}")
    return wrong
