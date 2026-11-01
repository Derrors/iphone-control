//! 守护进程核心：唯一持有设备的进程。共享状态、请求分发、快照。
//!
//! 并发模型（全部跑在主循环的 LocalSet 里，单线程协作）：
//! - 设备相关的状态在 [`Inner`] 里，一把锁串行访问：HID 会话、WDA 会话只能有一个持有者，
//!   也不会出现 toolkit 那种「长请求期间被巡检误判 WDA 挂了」的问题
//! - 每个控制请求一个任务，拿到锁才执行；巡检（[`crate::supervise`]）拿不到锁就跳过这一轮
//! - 耗时的事不持锁：启动 WDA（含挂开发者镜像，最长约 2 分钟）、重签、会话检查在后台跑，做完再持锁写结果
//! - `status` 读发布出来的快照（watch 通道），不碰锁
//! - 改变手机状态的请求排队超过 [`STALE`] 就放弃，免得点击落到已经变了的屏幕上

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ictl_device::{AppInfo, Device, DeviceError, DeviceInfo, ScreenSize, Tunnel};
use ictl_unlock::UnlockConfig;
use ictl_wda::{WdaClient, WdaRunner, http::DeviceTransport};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::eventlog::EventLog;
use crate::machine::{Backoff, DeviceState, Misses, MuxWatch};
use crate::notify::{self, Dedup, Event};
use crate::protocol::{Request, Response};
use crate::supervise::StartOutcome;

pub const TICK: Duration = Duration::from_secs(10);
/// 改变手机状态的请求排队超过这么久就放弃
pub const STALE: Duration = Duration::from_secs(30);
/// 长按、滑动的时长上限（秒）：HID 拖动按时长换算采样点，不设上限一个请求就能占住设备很久
pub const MAX_GESTURE_SECONDS: f64 = 10.0;
/// 等界面稳定、等元素出现的上限（毫秒）：持着设备锁等，不能太久
pub const MAX_WAIT_MS: u64 = 30_000;

/// 持久化到 `state.json` 的状态（不含秘密）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persisted {
    pub team_id: Option<String>,
    pub wda_bundle: Option<String>,
    /// 描述文件到期时间（Unix 秒）
    pub profile_expires: Option<u64>,
    pub screen: Option<ScreenSize>,
    pub last_resign: Option<String>,
}

impl Persisted {
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else { return Self::default() };
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            // 损坏时按空状态启动（会显示「WDA 还没签名」），但要留痕
            tracing::warn!("state.json 解析失败，按空状态启动：{e}");
            Self::default()
        })
    }

    pub(crate) fn save(&self, path: &Path) {
        let tmp = path.with_extension("json.tmp");
        let r = std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())
            .and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = r {
            tracing::warn!("保存 state.json 失败：{e}");
        }
    }

    /// 描述文件还剩多久；不知道返回 None
    pub fn profile_left(&self) -> Option<Duration> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        self.profile_expires.map(|e| Duration::from_secs(e.saturating_sub(now)))
    }
}

/// 快照：`iphone-control status` 显示
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub state: DeviceState,
    pub device: Option<DeviceInfo>,
    pub persisted: Persisted,
    pub auto_unlock: bool,
    pub unlock_disabled: bool,
    pub notify_configured: bool,
    pub log_tail: Vec<String>,
}

pub(crate) struct Inner {
    pub persisted: Persisted,
    pub state: DeviceState,
    pub device: Option<Device>,
    pub info: Option<DeviceInfo>,
    /// HID 和截图用的隧道（HID 会话归它所有）
    pub tunnel: Option<Tunnel>,
    pub runner: Option<WdaRunner>,
    pub wda: Option<WdaClient<DeviceTransport>>,
    /// 后台启动 WDA 的任务（含挂开发者镜像），巡检里收结果
    pub starting: Option<JoinHandle<StartOutcome>>,
    /// 开发者镜像连续挂载失败次数，到 3 次清缓存重新下载
    pub ddi_failures: u32,
    pub resigning: bool,
    pub wda_misses: Misses,
    pub backoff: Backoff,
    pub mux: MuxWatch,
    pub usbmuxd: Option<tokio::process::Child>,
    pub usbmuxd_started: Option<Instant>,
    /// usbmuxd 起来就退出时，下次重启不早于这个时间（退避）
    pub usbmuxd_retry_at: Option<Instant>,
    pub usbmuxd_delay: Duration,
    /// 连续列不出设备的次数（usbmuxd 抖一下不立刻当成离线）
    pub list_errors: u32,
    pub offline_since: Option<Instant>,
    /// 最近一次经本进程锁屏的时间（自动解锁要等锁屏过渡走完，见 [`crate::tools::LOCK_SETTLE`]）
    pub last_lock: Option<Instant>,
    /// HID 会话最近一次使用的时间；空闲超过 [`crate::supervise::HID_IDLE`] 就关掉（它持有屏幕媒体流）
    pub hid_used: Option<Instant>,
    pub dedup: Dedup,
    pub next_resign: Instant,
    pub next_session_check: Instant,
    pub apps: HashMap<bool, Vec<AppInfo>>,
}

pub struct Daemon {
    pub cfg: Config,
    pub log: EventLog,
    pub(crate) inner: Mutex<Inner>,
    snap: watch::Sender<Snapshot>,
}

/// 停掉 HID 会话（屏幕媒体流），隧道保留。媒体流不停的话手机一直以为「配对的电脑」在看屏幕：
/// 灵动岛常驻图标、不会自动锁屏，本进程退出、拔插数据线都清不掉（iOS 27 真机）
pub(crate) async fn close_hid(g: &mut Inner) {
    if let Some(t) = g.tunnel.as_mut() {
        t.close_hid().await;
    }
}

/// 丢掉隧道。一律走这里：先停 HID 媒体流再断（见 [`close_hid`]）
pub(crate) async fn drop_tunnel(g: &mut Inner) {
    if let Some(t) = g.tunnel.take() {
        t.close().await;
    }
}

pub(crate) fn wda_down(state: &DeviceState) -> String {
    format!("WDA 不可用({}),守护进程一般 1~2 分钟内会拉起;截图/点击/滑动/按键可照常用", state.describe())
}

impl Daemon {
    pub fn new(cfg: Config) -> Arc<Self> {
        let log = EventLog::new(&cfg.logs());
        let persisted = Persisted::load(&cfg.state_file());
        let now = Instant::now();
        let inner = Inner {
            persisted,
            state: DeviceState::Absent,
            device: None,
            info: None,
            tunnel: None,
            runner: None,
            wda: None,
            starting: None,
            ddi_failures: 0,
            resigning: false,
            wda_misses: Misses::default(),
            backoff: Backoff::default(),
            mux: MuxWatch::default(),
            usbmuxd: None,
            usbmuxd_started: None,
            usbmuxd_retry_at: None,
            usbmuxd_delay: Duration::ZERO,
            list_errors: 0,
            offline_since: Some(now),
            last_lock: None,
            hid_used: None,
            dedup: Dedup::default(),
            // 启动一分钟后再做第一次重签检查和会话验证，先让设备就绪
            next_resign: now + Duration::from_secs(60),
            next_session_check: now + Duration::from_secs(90),
            apps: HashMap::new(),
        };
        let unlock = UnlockConfig {
            passcode_file: cfg.passcode_file(),
            disabled_flag: cfg.unlock_disabled_flag(),
            timing: Default::default(),
        };
        let first = Snapshot {
            state: DeviceState::Absent,
            device: None,
            persisted: inner.persisted.clone(),
            auto_unlock: unlock.enabled(),
            unlock_disabled: unlock.disabled_by_failure(),
            notify_configured: cfg.notify.any(),
            log_tail: Vec::new(),
        };
        let (snap, _) = watch::channel(first);
        Arc::new(Self { cfg, log, inner: Mutex::new(inner), snap })
    }

    pub(crate) fn unlock_cfg(&self) -> UnlockConfig {
        UnlockConfig {
            passcode_file: self.cfg.passcode_file(),
            disabled_flag: self.cfg.unlock_disabled_flag(),
            timing: Default::default(),
        }
    }

    pub(crate) fn notify(&self, g: &mut Inner, e: Event) {
        if !g.dedup.should_send(&e, Instant::now()) {
            return;
        }
        let (title, body) = e.title_body();
        self.log.log(&format!("通知：{title}"));
        let cfg = self.cfg.notify.clone();
        if cfg.any() {
            tokio::spawn(async move {
                notify::send(&cfg, &title, &body).await;
            });
        }
    }

    pub(crate) fn set_state(&self, g: &mut Inner, s: DeviceState) {
        if g.state != s {
            self.log.log(&format!("状态：{}", s.describe()));
            g.state = s;
        }
    }

    /// 丢掉整个会话：runner、WDA 客户端、正在后台启动的任务、隧道（先停 HID 媒体流）
    pub(crate) async fn drop_session(&self, g: &mut Inner) {
        if let Some(h) = g.starting.take() {
            h.abort();
        }
        if let Some(r) = g.runner.take() {
            r.stop();
        }
        g.wda = None;
        drop_tunnel(g).await;
        g.wda_misses.reset();
    }

    pub(crate) async fn ensure_tunnel<'a>(
        &self,
        g: &'a mut Inner,
        device: &Device,
    ) -> Result<&'a mut Tunnel, DeviceError> {
        if g.tunnel.is_none() {
            g.tunnel = Some(device.open_tunnel().await?);
        }
        Ok(g.tunnel.as_mut().expect("刚建好"))
    }

    /// WDA 可用就返回客户端（本进程 runner 存活）
    pub(crate) fn wda<'a>(&self, g: &'a mut Inner) -> Option<&'a mut WdaClient<DeviceTransport>> {
        if g.runner.as_ref().is_some_and(|r| r.alive()) { g.wda.as_mut() } else { None }
    }

    pub(crate) fn require_wda<'a>(&self, g: &'a mut Inner) -> Result<&'a mut WdaClient<DeviceTransport>, String> {
        let msg = wda_down(&g.state);
        self.wda(g).ok_or(msg)
    }

    pub(crate) fn account(&self) -> ictl_sign::AccountDir {
        ictl_sign::AccountDir { dir: self.cfg.account_dir(), anisette_url: self.cfg.anisette_url.clone() }
    }

    /// 发布快照给不拿锁的读者（`status`）。日志尾巴在读的时候再取
    pub(crate) fn publish(&self, g: &Inner) {
        let unlock = self.unlock_cfg();
        self.snap.send_replace(Snapshot {
            state: g.state.clone(),
            device: g.info.clone(),
            persisted: g.persisted.clone(),
            auto_unlock: unlock.enabled(),
            unlock_disabled: unlock.disabled_by_failure(),
            notify_configured: self.cfg.notify.any(),
            log_tail: Vec::new(),
        });
    }

    // ------------------------------------------------------------------ 请求处理

    /// 处理一个控制请求。`queued_at`：请求到达的时间，用来放弃排队太久的操作
    pub async fn handle(self: &Arc<Self>, req: Request, queued_at: Instant) -> Response {
        // 不需要设备锁的请求
        match req {
            Request::Daemon => {
                let mut s = self.snap.borrow().clone();
                s.unlock_disabled = self.unlock_cfg().disabled_by_failure();
                s.auto_unlock = self.unlock_cfg().enabled();
                s.log_tail = self.log.tail(15);
                return Response::text(serde_json::to_string_pretty(&s).unwrap_or_default());
            }
            // 重签自己分段持锁：签名、安装要几十秒，期间不挡住别的请求
            Request::Resign { force } => {
                return self.resign_now(force).await.map(Response::text).unwrap_or_else(Response::error);
            }
            _ => {}
        }
        if let Err(e) = check_request(&req) {
            return Response::error(e);
        }

        let mut g = self.inner.lock().await;
        let g = &mut *g;
        if req.is_device_action() && queued_at.elapsed() > STALE {
            return Response::error(format!(
                "守护进程正忙,这个操作排队超过 {} 秒,已放弃执行(以免落到已经变化的屏幕上);先截图看看再重试",
                STALE.as_secs()
            ));
        }
        let r = match req {
            Request::Status => Ok(self.status_text(g).await),
            Request::Screenshot { region } => {
                let resp = self.screenshot(g, region).await.unwrap_or_else(Response::error);
                self.publish(g);
                return resp;
            }
            Request::ListElements { filter, limit } => self.list_elements(g, &filter, limit).await,
            Request::Tap { x, y } => self.tap(g, x, y).await,
            Request::TapElement { text, index } => self.tap_element(g, &text, index).await,
            Request::LongPress { x, y, seconds } => self.long_press(g, x, y, seconds).await,
            Request::Swipe { from, to, seconds } => self.swipe(g, from, to, seconds).await,
            Request::TypeText { text, submit } => self.type_text(g, &text, submit).await,
            Request::PressButton { button } => self.press_button(g, &button).await,
            Request::ListApps { include_system } => self
                .list_apps(g, include_system)
                .await
                .map(|a| a.iter().map(|a| format!("{}\t{}", a.name, a.bundle_id)).collect::<Vec<_>>().join("\n")),
            Request::LaunchApp { app } => self.launch_app(g, &app).await,
            Request::TerminateApp { app } => self.terminate_app(g, &app).await,
            Request::OpenUrl { url } => self.open_url(g, &url).await,
            Request::Settle { max_ms } => self.settle(g, Duration::from_millis(max_ms)).await,
            Request::WaitFor { text, timeout_ms } => self.wait_for(g, &text, Duration::from_millis(timeout_ms)).await,
            Request::Doctor => Ok(self.doctor(g).await),
            Request::UnlockReset => {
                let _ = std::fs::remove_file(self.cfg.unlock_disabled_flag());
                g.dedup.clear("unlock");
                Ok("已恢复自动解锁".into())
            }
            Request::Daemon | Request::Resign { .. } => unreachable!("上面已处理"),
        };
        self.publish(g);
        r.map(Response::text).unwrap_or_else(Response::error)
    }

    pub async fn shutdown(&self) {
        let mut g = self.inner.lock().await;
        self.log.log("收到停止信号,退出");
        // 先停 HID 媒体流、runner（经 usbmuxd），再停 usbmuxd
        self.drop_session(&mut g).await;
        if let Some(mut c) = g.usbmuxd.take() {
            let _ = c.kill().await;
        }
    }
}

/// 参数检查（与设备无关，不必拿锁）
fn check_request(req: &Request) -> Result<(), String> {
    match req {
        Request::Settle { max_ms } if *max_ms > MAX_WAIT_MS => return Err(format!("max_ms 不能超过 {MAX_WAIT_MS}")),
        Request::WaitFor { timeout_ms, .. } if *timeout_ms > MAX_WAIT_MS => {
            return Err(format!("timeout 不能超过 {} 秒", MAX_WAIT_MS / 1000));
        }
        Request::WaitFor { text, .. } if text.is_empty() => return Err("text 不能为空".into()),
        _ => {}
    }
    let seconds = match req {
        Request::LongPress { seconds, .. } | Request::Swipe { seconds, .. } => *seconds,
        _ => return Ok(()),
    };
    if !seconds.is_finite() || !(0.0..=MAX_GESTURE_SECONDS).contains(&seconds) {
        return Err(format!("seconds 要在 0 到 {MAX_GESTURE_SECONDS} 之间"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_seconds_are_bounded() {
        let swipe = |s| Request::Swipe { from: (0.0, 0.0), to: (1.0, 1.0), seconds: s };
        assert!(check_request(&swipe(0.3)).is_ok());
        assert!(check_request(&swipe(1e6)).is_err());
        assert!(check_request(&swipe(-1.0)).is_err());
        assert!(check_request(&swipe(f64::NAN)).is_err());
        assert!(check_request(&Request::LongPress { x: 0.0, y: 0.0, seconds: 10.0 }).is_ok());
    }

    #[test]
    fn corrupt_state_file_loads_as_default() {
        let p = std::env::temp_dir().join(format!("ictl-state-{}.json", std::process::id()));
        std::fs::write(&p, b"{not json").unwrap();
        assert!(Persisted::load(&p).wda_bundle.is_none());
        std::fs::remove_file(&p).unwrap();
    }
}
