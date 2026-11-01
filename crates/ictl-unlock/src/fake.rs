//! 单测用的假锁屏：按 toolkit 实测到的 WDA 行为模拟锁屏界面。
//!
//! - 键盘元素一直在元素树里（不管是否显示）
//! - 只有「按 Home 后上滑」之后键盘才真正显示，显示前按键落空
//! - 整串输完：对了就解锁，错了算一次输错并清空

use std::sync::Mutex;
use std::time::Duration;

use ictl_wda::http::Transport;
use serde_json::{Value, json};

const LOCK_XML: &str = include_str!("../../ictl-wda/tests/fixtures/lockscreen.xml");
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
    press_count: u32,
    drop_press: Option<u32>,
}

pub struct FakeLockScreen {
    code: Vec<u8>,
    st: Mutex<State>,
}

impl FakeLockScreen {
    pub fn new(code: &str) -> Self {
        let st = State { locked: true, idle: (10.0, 2.0), ..Default::default() };
        Self { code: code.bytes().map(|b| b - b'0').collect(), st: Mutex::new(st) }
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

    /// 第 n 次有效按键（从 1 数）被键盘吞掉
    pub fn drop_press(&self, n: u32) {
        self.st.lock().unwrap().drop_press = Some(n);
    }

    fn handle(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut s = self.st.lock().unwrap();
        let path = path.strip_prefix("/session/S").unwrap_or(path);
        let v = match (method, path) {
            ("GET", "/wda/locked") => json!(s.locked),
            ("GET", "/wda/screen") => json!({"screenSize": {"width": 430, "height": 932}, "scale": 3}),
            ("POST", "/session") => return (200, json!({"sessionId": "S", "value": {}})),
            ("GET", "/appium/settings") => json!({"waitForIdleTimeout": s.idle.0, "animationCoolOffTimeout": s.idle.1}),
            ("POST", "/appium/settings") => {
                let b = &body.unwrap()["settings"];
                s.idle = (b["waitForIdleTimeout"].as_f64().unwrap(), b["animationCoolOffTimeout"].as_f64().unwrap());
                Value::Null
            }
            ("POST", "/wda/pressButton") => {
                s.shown = false;
                s.entered.clear();
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
                if is_swipe {
                    if s.swipes_to_ignore > 0 {
                        s.swipes_to_ignore -= 1;
                    } else {
                        s.shown = true;
                    }
                } else {
                    s.presses_per_action_max = s.presses_per_action_max.max(downs);
                    let (x, y) = (steps[0]["x"].as_f64().unwrap() as i64, steps[0]["y"].as_f64().unwrap() as i64);
                    let digit = KEYS.iter().position(|k| *k == (x, y)).expect("按在键盘以外") as u8;
                    if s.shown {
                        s.press_count += 1;
                        if s.drop_press != Some(s.press_count) {
                            s.entered.push(digit);
                            s.digits_typed += 1;
                        }
                        if s.entered.len() == self.code.len() {
                            if s.entered == self.code {
                                s.locked = false;
                            } else {
                                s.failed_attempts += 1;
                            }
                            s.entered.clear();
                        }
                    }
                }
                Value::Null
            }
            ("GET", p) if p.starts_with("/source") => json!(LOCK_XML),
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
