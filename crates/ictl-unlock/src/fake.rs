//! 单测用的假锁屏：按 toolkit 实测到的 WDA 行为模拟锁屏界面。
//!
//! - 键盘元素一直在元素树里（不管是否显示）
//! - 只有「按 Home 后上滑」之后键盘才真正显示，显示前按键落空
//! - 整串输完：对了就解锁，错了算一次输错并清空
//! - 按 Home 不清空半截输入（iOS 27 真机）；右下角删除键每按一下删一位
//! - 可选熄屏：熄屏时上滑无效，要先按 Home 点亮；可设成 XCTest 的 home 点不亮（iPhone 12 mini / iOS 26.4.1 真机），
//!   只有 IOHID 的 AC Home 能点亮
//! - 可选 iPad 模式（iPad mini 5 / iPadOS 26.6.1 真机）：横屏、Touch ID。上滑后是「触控 ID」提示，点「使用密码」才出键盘；
//!   SpringBoard 的树是竖屏坐标，点击是屏幕坐标。点到别处什么都不发生（记在 `stray_taps`）

use std::sync::Mutex;
use std::time::Duration;

use ictl_wda::http::Transport;
use serde_json::{Value, json};

const LOCK_XML: &str = include_str!("../../ictl-wda/tests/fixtures/lockscreen.xml");
const HOME_XML: &str = include_str!("../../ictl-wda/tests/fixtures/home.xml");
/// 与 lockscreen.xml 里各键的中心点一致
const KEYS: [(i64, i64); 10] = [
    (215, 695),
    (103, 371),
    (215, 371),
    (327, 371),
    (103, 479),
    (215, 479),
    (327, 479),
    (103, 587),
    (215, 587),
    (327, 587),
];
/// lockscreen.xml 里右下角按钮（有输入时是「删除」）的中心
const DELETE: (i64, i64) = (327, 857);

const IPAD_TOUCHID_XML: &str = include_str!("../../ictl-wda/tests/fixtures/ipad-touchid-landscape.xml");
const IPAD_PIN_XML: &str = include_str!("../../ictl-wda/tests/fixtures/ipad-passcode-landscape.xml");
/// Home 键在左的横屏：树里的坐标和上面中心对称，换算到屏幕上键位不变
const IPAD_TOUCHID_LEFT_XML: &str = include_str!("../../ictl-wda/tests/fixtures/ipad-touchid-landscape-home-left.xml");
/// iPad 竖屏锁屏（上滑之前）。去掉「按下主屏幕按钮以解锁」那行就是熄屏、有通知时的样子：认不出是锁屏
const IPAD_LOCK_XML: &str = include_str!("../../ictl-wda/tests/fixtures/ipad-lockscreen-portrait.xml");
const IPAD_PIN_LEFT_XML: &str = include_str!("../../ictl-wda/tests/fixtures/ipad-passcode-landscape-home-left.xml");
/// ipad-passcode-landscape.xml 里各键按 Home 键在右的横屏换算到屏幕上的中心点（真机截图对得上）
const IPAD_KEYS: [(i64, i64); 10] = [
    (512, 586),
    (406, 282),
    (512, 282),
    (618, 282),
    (406, 384),
    (512, 384),
    (618, 384),
    (406, 484),
    (512, 484),
    (618, 484),
];
const IPAD_DELETE: (i64, i64) = (618, 586);
const IPAD_USE_PASSCODE: (i64, i64) = (512, 430);

#[derive(Debug, Clone, Default)]
pub struct State {
    pub locked: bool,
    pub shown: bool,
    pub entered: Vec<u8>,
    pub idle: (f64, f64),
    pub idle_during_unlock: Vec<(f64, f64)>,
    pub presses_per_action_max: usize,
    pub requests_to_actions: usize,
    pub failed_attempts: u32,
    pub digits_typed: u32,
    swipes_to_ignore: u32,
    swipe_errors: u32,
    press_count: u32,
    drop_press: Option<u32>,
    double_press: Option<u32>,
    /// WDA 在锁屏时启动：`/wda/locked` 一直报 false
    stale_wda_lock: bool,
    /// 第 n 次有效按键之后，取元素树和点击都报错（隧道断了）
    broken_after_press: Option<u32>,
    /// 屏幕熄灭：上滑无效
    asleep: bool,
    /// XCTest 的 home 点不亮屏幕
    xctest_home_dead: bool,
    pub hid_events: u32,
    /// iPad 模式：点过「使用密码」，键盘出来了
    pub passcode_requested: bool,
    /// iPad 模式：点在键和按钮以外的次数
    pub stray_taps: u32,
}

pub struct FakeLockScreen {
    code: Vec<u8>,
    st: Mutex<State>,
    /// iPad 模式时 WDA 报的方向（`/orientation`）；None 是 iPhone 模式，`/orientation` 报 404
    ipad: Option<&'static str>,
    /// iPad 横屏时 Home 键在左（默认在右）
    ipad_home_left: bool,
    /// iPad 模式：上滑之前的锁屏上没有「按下主屏幕按钮以解锁」（熄屏、有通知时），树里没有锁的标记
    ipad_hint_hidden: bool,
}

impl FakeLockScreen {
    pub fn new(code: &str) -> Self {
        let st = State { locked: true, idle: (10.0, 2.0), ..Default::default() };
        Self {
            code: code.bytes().map(|b| b - b'0').collect(),
            st: Mutex::new(st),
            ipad: None,
            ipad_home_left: false,
            ipad_hint_hidden: false,
        }
    }

    /// iPad 横屏（Home 键在右）、Touch ID；WDA 报的方向是 `orientation`
    pub fn ipad_touch_id(code: &str, orientation: &'static str) -> Self {
        Self { ipad: Some(orientation), ..Self::new(code) }
    }

    /// 同上，但 Home 键在左。真机上 WDA 照样报 `LANDSCAPE`
    pub fn ipad_touch_id_home_left(code: &str, orientation: &'static str) -> Self {
        Self { ipad_home_left: true, ..Self::ipad_touch_id(code, orientation) }
    }

    /// iPad 锁屏上没有解锁提示（见 `ipad_hint_hidden`）
    pub fn ipad_lock_screen_without_hint(code: &str, orientation: &'static str) -> Self {
        Self { ipad_hint_hidden: true, ..Self::ipad_touch_id(code, orientation) }
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    pub fn unlock_now(&self) {
        self.st.lock().unwrap().locked = false;
    }

    pub fn swipes_ignored(&self, n: u32) {
        self.st.lock().unwrap().swipes_to_ignore = n;
    }

    /// 键盘已打开、留着 n 位残留输入（别人按了一半）
    pub fn leave_residual(&self, digits: &[u8]) {
        let mut s = self.st.lock().unwrap();
        s.shown = true;
        s.entered = digits.to_vec();
    }

    /// 接下来 n 次上滑 WDA 报 500（真机：熄屏锁屏刚唤醒时 `point.x != INFINITY`）
    pub fn swipe_errors(&self, n: u32) {
        self.st.lock().unwrap().swipe_errors = n;
    }

    /// 第 n 次有效按键（从 1 数）被键盘吞掉
    pub fn drop_press(&self, n: u32) {
        self.st.lock().unwrap().drop_press = Some(n);
    }

    /// 第 n 次有效按键被记成两下
    pub fn double_press(&self, n: u32) {
        self.st.lock().unwrap().double_press = Some(n);
    }

    pub fn stale_wda_lock(&self) {
        self.st.lock().unwrap().stale_wda_lock = true;
    }

    /// 屏幕熄灭；`xctest_home_dead` 时只有 IOHID 的 AC Home 能点亮
    pub fn asleep(&self, xctest_home_dead: bool) {
        let mut s = self.st.lock().unwrap();
        s.asleep = true;
        s.xctest_home_dead = xctest_home_dead;
    }

    pub fn broken_after_press(&self, n: u32) {
        self.st.lock().unwrap().broken_after_press = Some(n);
    }

    fn handle(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut s = self.st.lock().unwrap();
        let path = path.strip_prefix("/session/S").unwrap_or(path);
        if s.broken_after_press.is_some_and(|n| s.press_count >= n)
            && (path == "/actions" || path.starts_with("/source"))
        {
            return (500, json!({"value": {"error": "unknown error", "message": "连接断了"}}));
        }
        let v = match (method, path) {
            ("GET", "/wda/locked") => json!(s.locked && !s.stale_wda_lock),
            ("GET", "/wda/activeAppInfo") => json!({"bundleId": "com.apple.springboard"}),
            ("GET", "/wda/screen") if self.ipad.is_some() => {
                json!({"screenSize": {"width": 1024, "height": 768}, "scale": 2})
            }
            ("GET", "/wda/screen") => json!({"screenSize": {"width": 430, "height": 932}, "scale": 3}),
            ("GET", "/orientation") if self.ipad.is_some() => json!(self.ipad),
            ("POST", "/session") => return (200, json!({"sessionId": "S", "value": {}})),
            ("GET", "/appium/settings") => json!({"waitForIdleTimeout": s.idle.0, "animationCoolOffTimeout": s.idle.1}),
            ("POST", "/appium/settings") => {
                let b = &body.unwrap()["settings"];
                s.idle = (b["waitForIdleTimeout"].as_f64().unwrap(), b["animationCoolOffTimeout"].as_f64().unwrap());
                Value::Null
            }
            // iOS 27：Home 只唤醒/收起键，半截输入还在，键盘界面也还在（键隐藏但仍能按）
            ("POST", "/wda/pressButton") => {
                if !s.xctest_home_dead {
                    s.asleep = false;
                }
                Value::Null
            }
            ("POST", "/wda/performIoHidEvent") => {
                s.hid_events += 1;
                let b = body.unwrap();
                if (b["page"].as_u64(), b["usage"].as_u64()) == (Some(0x0C), Some(0x223)) {
                    s.asleep = false;
                }
                Value::Null
            }
            ("POST", "/actions") => {
                s.requests_to_actions += 1;
                let idle = s.idle;
                s.idle_during_unlock.push(idle);
                let steps = body.unwrap()["actions"][0]["actions"].as_array().unwrap().clone();
                let downs = steps.iter().filter(|a| a["type"] == "pointerDown").count();
                let is_swipe =
                    steps.iter().any(|a| a["type"] == "pointerMove" && a["duration"].as_u64().unwrap_or(0) > 0);
                if is_swipe && s.swipe_errors > 0 {
                    s.swipe_errors -= 1;
                    let msg = "Invalid parameter not satisfying: point.x != INFINITY && point.y != INFINITY";
                    return (500, json!({"value": {"error": "unknown error", "message": msg}}));
                }
                if is_swipe {
                    if s.asleep {
                        // 熄屏时上滑什么都不发生
                    } else if s.swipes_to_ignore > 0 {
                        s.swipes_to_ignore -= 1;
                    } else {
                        s.shown = true;
                    }
                } else {
                    s.presses_per_action_max = s.presses_per_action_max.max(downs);
                    let (x, y) = (steps[0]["x"].as_f64().unwrap() as i64, steps[0]["y"].as_f64().unwrap() as i64);
                    let (keys, delete) = if self.ipad.is_some() { (IPAD_KEYS, IPAD_DELETE) } else { (KEYS, DELETE) };
                    // iPad：键盘要点「使用密码」才出来
                    let pad_shown = s.shown && (self.ipad.is_none() || s.passcode_requested);
                    if self.ipad.is_some() && (x, y) == IPAD_USE_PASSCODE {
                        if s.shown {
                            s.passcode_requested = true;
                        }
                        return (200, json!({"value": null, "sessionId": "S"}));
                    }
                    if (x, y) == delete {
                        if pad_shown {
                            s.entered.pop();
                        }
                        return (200, json!({"value": null, "sessionId": "S"}));
                    }
                    let Some(digit) = keys.iter().position(|k| *k == (x, y)) else {
                        assert!(self.ipad.is_some(), "按在键盘以外：{x},{y}");
                        s.stray_taps += 1;
                        return (200, json!({"value": null, "sessionId": "S"}));
                    };
                    let digit = digit as u8;
                    if pad_shown {
                        s.press_count += 1;
                        let times = match s.press_count {
                            n if s.drop_press == Some(n) => 0,
                            n if s.double_press == Some(n) => 2,
                            _ => 1,
                        };
                        for _ in 0..times {
                            s.entered.push(digit);
                            s.digits_typed += 1;
                            if s.entered.len() == self.code.len() {
                                if s.entered == self.code {
                                    s.locked = false;
                                    s.shown = false;
                                    s.passcode_requested = false;
                                } else {
                                    s.failed_attempts += 1;
                                }
                                s.entered.clear();
                            }
                        }
                    }
                }
                Value::Null
            }
            ("GET", p) if p.starts_with("/source") => {
                if s.locked {
                    let field = format!("共{}位数，已输入{}位", self.code.len(), s.entered.len());
                    let pin = s.shown && s.passcode_requested;
                    if self.ipad_hint_hidden && !s.shown {
                        return (
                            200,
                            json!({"value": IPAD_LOCK_XML.replace("call-to-action-label", "x"), "sessionId": "S"}),
                        );
                    }
                    let xml = match (self.ipad, self.ipad_home_left, pin) {
                        (None, ..) => LOCK_XML,
                        (Some(_), false, true) => IPAD_PIN_XML,
                        (Some(_), false, false) => IPAD_TOUCHID_XML,
                        (Some(_), true, true) => IPAD_PIN_LEFT_XML,
                        (Some(_), true, false) => IPAD_TOUCHID_LEFT_XML,
                    };
                    json!(xml.replace("共6位数，已输入0位", &field))
                } else {
                    json!(HOME_XML)
                }
            }
            // iPad 的「触控 ID」提示页上没有密码栏
            ("POST", "/elements") if self.ipad.is_some() && !(s.shown && s.passcode_requested) => json!([]),
            ("POST", "/elements") => json!([{"ELEMENT": "E1"}]),
            ("GET", "/element/E1/attribute/value") => {
                json!(format!("共{}位数，已输入{}位", self.code.len(), s.entered.len()))
            }
            _ => return (404, json!({"value": {"error": "unknown command", "message": path}})),
        };
        (200, json!({"value": v, "sessionId": "S"}))
    }
}

impl Transport for &FakeLockScreen {
    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        _: Duration,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        let body = body.and_then(|b| serde_json::from_slice(&b).ok());
        let (status, v) = self.handle(method, path, body);
        Ok((status, v.to_string().into_bytes()))
    }
}
