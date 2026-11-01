//! 守护进程核心：唯一持有设备的进程。每 10 秒跑一次状态机，同时处理控制接口的请求。
//!
//! 所有设备操作都在同一把锁下串行执行：HID 会话、WDA 会话只能有一个持有者；
//! 也因此不会出现 toolkit 那种「长请求期间被巡检误判 WDA 挂了」的问题。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use ictl_device::{AppInfo, Button, Device, DeviceError, DeviceInfo, HidSession, ScreenSize, Tunnel, apps};
use ictl_unlock::{UnlockConfig, UnlockError};
use ictl_wda::{WdaClient, WdaError, WdaRunner, elements, http::DeviceTransport, swipe_steps};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::eventlog::EventLog;
use crate::imaging::{self, fmt_g};
use crate::machine::{Backoff, DeviceState, Misses, MuxWatch};
use crate::notify::{self, Dedup, Event};
use crate::protocol::{Request, Response};

pub const TICK: Duration = Duration::from_secs(10);
/// 正常 2～10 秒就绪。手机重启后未授权 automation 时，testmanagerd 握手每次卡 30 秒、要重试一两次，
/// 60 秒常常等不到 runner 启动，也就认不出「需要输入密码」
const WDA_START_TIMEOUT: Duration = Duration::from_secs(120);
/// HID 会话空闲这么久就关掉。它持有屏幕媒体流，开着时 iOS 当作屏幕被远程查看：不会自动锁屏，
/// 还会弹「viewed remotely」通知。重开约 0.5 秒
const HID_IDLE: Duration = Duration::from_secs(30);
/// 锁屏后至少等这么久再自动解锁：锁屏过渡没走完就唤醒，WDA 第一下上滑卡约 16 秒后报 `point.x != INFINITY`
const LOCK_SETTLE: Duration = Duration::from_secs(3);
const SYSFS_USB: &str = "/sys/bus/usb/devices";

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
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self, path: &Path) {
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

struct Inner {
    persisted: Persisted,
    state: DeviceState,
    device: Option<Device>,
    info: Option<DeviceInfo>,
    tunnel: Option<Tunnel>,
    hid: Option<HidSession>,
    runner: Option<WdaRunner>,
    wda: Option<WdaClient<DeviceTransport>>,
    wda_misses: Misses,
    backoff: Backoff,
    mux: MuxWatch,
    usbmuxd: Option<tokio::process::Child>,
    offline_since: Option<Instant>,
    /// 最近一次经本进程锁屏的时间（自动解锁要等锁屏过渡走完，见 [`LOCK_SETTLE`]）
    last_lock: Option<Instant>,
    /// HID 会话最近一次使用的时间；空闲超过 [`HID_IDLE`] 就关掉（它持有屏幕媒体流）
    hid_used: Option<Instant>,
    dedup: Dedup,
    next_resign: Instant,
    next_session_check: Instant,
    apps: HashMap<bool, Vec<AppInfo>>,
}

pub struct Daemon {
    pub cfg: Config,
    pub log: EventLog,
    inner: Mutex<Inner>,
}

/// 先停 HID 会话的屏幕媒体流再丢掉（要在隧道断开之前）。直接丢的话手机一直以为「配对的电脑」在看屏幕：
/// 灵动岛常驻图标、不会自动锁屏，本进程退出后也不消失（iOS 27 真机，要拔插数据线才清掉）
async fn close_hid(g: &mut Inner) {
    if let Some(h) = g.hid.take() {
        let _ = tokio::time::timeout(Duration::from_secs(3), h.close()).await;
    }
}

fn wda_down(state: &DeviceState) -> String {
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
            hid: None,
            runner: None,
            wda: None,
            wda_misses: Misses::default(),
            backoff: Backoff::default(),
            mux: MuxWatch::default(),
            usbmuxd: None,
            offline_since: Some(now),
            last_lock: None,
            hid_used: None,
            dedup: Dedup::default(),
            // 启动一分钟后再做第一次重签检查和会话验证，先让设备就绪
            next_resign: now + Duration::from_secs(60),
            next_session_check: now + Duration::from_secs(90),
            apps: HashMap::new(),
        };
        Arc::new(Self { cfg, log, inner: Mutex::new(inner) })
    }

    fn unlock_cfg(&self) -> UnlockConfig {
        UnlockConfig {
            passcode_file: self.cfg.passcode_file(),
            disabled_flag: self.cfg.unlock_disabled_flag(),
            timing: Default::default(),
        }
    }

    fn notify(&self, g: &mut Inner, e: Event) {
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

    // ------------------------------------------------------------------ usbmuxd

    async fn ensure_usbmuxd(&self, g: &mut Inner, restart: bool) {
        if !self.cfg.manage_usbmuxd {
            return;
        }
        let alive = g.usbmuxd.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        if alive && !restart {
            return;
        }
        if let Some(mut c) = g.usbmuxd.take() {
            let _ = c.kill().await;
        }
        // 经旧 usbmuxd 建立的连接（隧道、runner、WDA）都已失效
        self.drop_session(g).await;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.cfg.logs().join("usbmuxd.log"));
        let mut cmd = tokio::process::Command::new(&self.cfg.usbmuxd_bin);
        cmd.args(["-U", "root", "-f"]).kill_on_drop(true);
        if let Ok(f) = log
            && let Ok(f2) = f.try_clone()
        {
            cmd.stdout(f).stderr(f2);
        }
        match cmd.spawn() {
            Ok(c) => {
                g.usbmuxd = Some(c);
                self.log.log("usbmuxd 已(重新)启动");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => self.log.log(&format!("启动 usbmuxd 失败：{e}")),
        }
    }

    // ------------------------------------------------------------------ 状态机

    async fn drop_session(&self, g: &mut Inner) {
        if let Some(r) = g.runner.take() {
            r.stop();
        }
        g.wda = None;
        close_hid(g).await;
        g.tunnel = None;
        g.wda_misses.reset();
    }

    fn set_state(&self, g: &mut Inner, s: DeviceState) {
        if g.state != s {
            self.log.log(&format!("状态：{}", s.describe()));
            g.state = s;
        }
    }

    pub async fn tick(&self) {
        let mut g = self.inner.lock().await;
        let g = &mut *g;
        self.ensure_usbmuxd(g, false).await;

        if g.hid.is_some() && g.hid_used.is_none_or(|t| t.elapsed() >= HID_IDLE) {
            close_hid(g).await;
            tracing::info!("HID 会话空闲，已关闭（停止屏幕媒体流）");
        }

        let devices = ictl_device::list_devices().await.unwrap_or_default();
        let target = match &self.cfg.udid {
            Some(u) => devices.into_iter().find(|d| &d.udid == u),
            None => devices.into_iter().next(),
        };
        let usb_present = ictl_device::apple_usb_present(Path::new(SYSFS_USB));
        if g.mux.observe(usb_present, target.is_some()) {
            self.log.log("USB 上有 Apple 设备但 usbmuxd 看不到,重启 usbmuxd");
            self.ensure_usbmuxd(g, true).await;
        }

        match target {
            None => self.offline(g).await,
            Some(d) => self.online(g, &d.udid).await,
        }

        let now = Instant::now();
        if now >= g.next_resign {
            g.next_resign = now + self.cfg.resign_check;
            if g.device.is_some()
                && g.persisted.wda_bundle.is_some()
                && let Err(e) = self.resign(g, false).await
            {
                self.notify(g, Event::ResignFailed(e));
            }
        }
        if now >= g.next_session_check {
            g.next_session_check = now + self.cfg.session_check;
            self.check_session(g).await;
        }
    }

    async fn offline(&self, g: &mut Inner) {
        let now = Instant::now();
        if g.device.take().is_some() {
            self.log.log("手机离线");
            self.drop_session(g).await;
            g.offline_since = Some(now);
            g.apps.clear();
        }
        self.set_state(g, DeviceState::Absent);
        let since = *g.offline_since.get_or_insert(now);
        let alert = self.cfg.offline_alert;
        if !alert.is_zero() && now.duration_since(since) >= alert {
            self.notify(g, Event::DeviceOffline { minutes: alert.as_secs() / 60 });
        }
    }

    async fn online(&self, g: &mut Inner, udid: &str) {
        if g.device.as_ref().map(|d| d.udid.as_str()) != Some(udid) {
            self.drop_session(g).await;
            match Device::find(Some(udid)).await {
                Ok(d) => {
                    g.info = d.info().await.ok();
                    let model = g
                        .info
                        .as_ref()
                        .map(|i| format!(" {} iOS {}", i.product_type, i.product_version))
                        .unwrap_or_default();
                    self.log.log(&format!("手机在线{model}"));
                    g.device = Some(d);
                }
                Err(e) => {
                    self.log.log(&format!("打开设备失败：{e}"));
                    return;
                }
            }
            g.backoff.reset();
            if let Some(since) = g.offline_since.take()
                && g.dedup.was_sent("offline")
            {
                g.dedup.clear("offline");
                self.notify(g, Event::DeviceOnline { minutes: since.elapsed().as_secs() / 60 });
            }
        }
        let device = g.device.clone().expect("刚设置过");

        if !device.paired().await.unwrap_or(false) {
            return self.set_state(g, DeviceState::NeedsTrust);
        }
        if g.state != DeviceState::Ready && !device.developer_mode_enabled().await.unwrap_or(true) {
            return self.set_state(g, DeviceState::DeveloperModeOff);
        }
        let Some(bundle) = g.persisted.wda_bundle.clone() else {
            return self.set_state(g, DeviceState::NeedsSigning);
        };

        // WDA 健康：本进程的 runner 还活着，且 /status ready
        let runner_alive = g.runner.as_ref().is_some_and(|r| r.alive());
        let ready = match (&g.wda, runner_alive) {
            (Some(w), true) => w.ready().await.unwrap_or(false),
            _ => false,
        };
        if ready {
            g.wda_misses.reset();
            if g.state != DeviceState::Ready {
                if g.state == DeviceState::NeedsAutomationPin || g.dedup.was_sent("pin") {
                    g.dedup.clear("pin");
                    self.notify(g, Event::WdaRecovered);
                }
                self.set_state(g, DeviceState::Ready);
                g.backoff.reset();
                if let Some(w) = g.wda.as_mut()
                    && let Ok(s) = w.screen_size().await
                {
                    g.persisted.screen = Some(s);
                    g.persisted.save(&self.cfg.state_file());
                }
            }
            return;
        }
        if runner_alive && !g.wda_misses.hit() {
            return;
        }
        if g.runner.is_some() {
            self.log.log("WDA 不可用");
            // 隧道和 HID 也一起丢：WDA 掉线常常是底层连接断了（例如 usbmuxd 重启），
            // 留着旧隧道，下次查镜像就报 socket io failed 并白白退避 60 秒
            self.drop_session(g).await;
        }
        g.wda_misses.reset();
        if !g.backoff.ready(Instant::now()) {
            return;
        }
        self.start_wda(g, &device, &bundle).await;
    }

    async fn ensure_tunnel<'a>(&self, g: &'a mut Inner, device: &Device) -> Result<&'a mut Tunnel, DeviceError> {
        if g.tunnel.is_none() {
            g.tunnel = Some(device.open_tunnel().await?);
        }
        Ok(g.tunnel.as_mut().expect("刚建好"))
    }

    async fn start_wda(&self, g: &mut Inner, device: &Device, bundle: &str) {
        self.set_state(g, DeviceState::WdaStarting { last_error: None });
        // 手机重启后开发者镜像会掉；已挂载时只是一次查询
        let ddi_cache = self.cfg.ddi_cache();
        let ddi_url = self.cfg.ddi_url.clone();
        let ddi: Result<Option<String>, DeviceError> = async {
            let t = self.ensure_tunnel(g, device).await?;
            if t.ddi_mounted().await?.is_some() {
                return Ok(None);
            }
            let dir = ictl_device::ddi::ensure_cache(&ddi_cache, &ddi_url).await?;
            Ok(Some(t.mount_ddi(&dir).await?))
        }
        .await;
        match ddi {
            Ok(Some(v)) => self.log.log(&format!("开发者镜像已挂载 {v}")),
            Ok(None) => {}
            Err(DeviceError::PasswordProtected) => {
                g.tunnel = None;
                return self.set_state(g, DeviceState::NeedsUnlock);
            }
            Err(e) => {
                g.tunnel = None;
                g.backoff.failed(Instant::now());
                return self.set_state(g, DeviceState::WdaStarting { last_error: Some(format!("开发者镜像：{e}")) });
            }
        }

        self.log.log("启动 WDA");
        match WdaRunner::start(device.provider(), bundle, WDA_START_TIMEOUT).await {
            Ok((runner, client)) => {
                g.runner = Some(runner);
                g.wda = Some(client);
            }
            Err(WdaError::NeedsAutomationPin) => {
                g.backoff.waiting_for_pin(Instant::now());
                self.set_state(g, DeviceState::NeedsAutomationPin);
                self.notify(g, Event::NeedsAutomationPin);
            }
            Err(e) => {
                g.backoff.failed(Instant::now());
                self.log.log(&format!("WDA 启动失败：{e}"));
                self.set_state(g, DeviceState::WdaStarting { last_error: Some(e.to_string()) });
            }
        }
    }

    // ------------------------------------------------------------------ 重签与会话

    fn account(&self) -> ictl_sign::AccountDir {
        ictl_sign::AccountDir { dir: self.cfg.account_dir(), anisette_url: self.cfg.anisette_url.clone() }
    }

    async fn check_session(&self, g: &mut Inner) {
        match self.account().session().await {
            Ok(_) => g.dedup.clear("session"),
            Err(ictl_sign::SignError::NotLoggedIn) => {}
            Err(e) => {
                self.log.log(&format!("Apple ID 会话检查失败：{e}"));
                self.notify(g, Event::SessionExpired(e.to_string()));
            }
        }
    }

    async fn resign(&self, g: &mut Inner, force: bool) -> Result<String, String> {
        if !force
            && g.persisted.wda_bundle.is_some()
            && let Some(left) = g.persisted.profile_left()
            && left > self.cfg.resign_min
        {
            return Ok(format!("描述文件还剩 {} 小时,无需重签", left.as_secs() / 3600));
        }
        let device = g.device.clone().ok_or("手机未连接")?;
        self.log.log("开始重签");
        let account = self.account();
        let name = g
            .info
            .as_ref()
            .map(|i| i.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "iphone-control".into());
        let (wda_dir, unsigned, signed_path) =
            (self.cfg.wda_dir(), self.cfg.wda_unsigned.clone(), self.cfg.wda_signed());
        // plume_core 的签名 future 不是 Send（持有 &mut dyn FnMut 和非 Send 的私钥），放到阻塞线程里 block_on
        let r: Result<ictl_sign::Signed, String> = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                let session = account.session().await.map_err(|e| e.to_string())?;
                session.ensure_device(&device.udid, &name).await.map_err(|e| e.to_string())?;
                std::fs::create_dir_all(&wda_dir).map_err(|e| e.to_string())?;
                let tmp = wda_dir.join("WDA-signing.ipa");
                let signed = session.sign_ipa(&unsigned, &tmp, &device.udid).await.map_err(|e| e.to_string())?;
                device.install(&tmp).await.map_err(|e| format!("安装到手机失败(手机锁屏?)：{e}"))?;
                std::fs::rename(&tmp, &signed_path).map_err(|e| e.to_string())?;
                Ok(signed)
            })
        })
        .await
        .unwrap_or_else(|e| Err(format!("签名任务异常退出：{e}")));
        match r {
            Ok(s) => {
                let exp = s.profile.expires.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                g.persisted.team_id = Some(s.team_id);
                g.persisted.wda_bundle = Some(s.bundle_id);
                g.persisted.profile_expires = Some(exp);
                g.persisted.last_resign = Some(format!("{} 成功", crate::eventlog::timestamp()));
                g.persisted.save(&self.cfg.state_file());
                // 新包装上后旧的 runner 会失效，下一轮巡检重新拉起
                self.drop_session(g).await;
                g.backoff.reset();
                g.dedup.clear("resign");
                let msg = format!("重签完成,新描述文件到期 {} UTC", crate::eventlog::format_utc(exp));
                self.log.log(&msg);
                Ok(msg)
            }
            Err(e) => {
                g.persisted.last_resign = Some(format!("{} 失败：{e}", crate::eventlog::timestamp()));
                g.persisted.save(&self.cfg.state_file());
                self.log.log(&format!("重签失败：{e}"));
                Err(e)
            }
        }
    }

    // ------------------------------------------------------------------ 请求处理

    pub async fn handle(&self, req: Request) -> Response {
        let mut g = self.inner.lock().await;
        let g = &mut *g;
        let r = match req {
            Request::Status => Ok(self.status_text(g).await),
            Request::Screenshot { region } => return self.screenshot(g, region).await.unwrap_or_else(Response::error),
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
            Request::Daemon => Ok(serde_json::to_string_pretty(&self.snapshot(g)).unwrap_or_default()),
            Request::Doctor => Ok(self.doctor(g).await),
            Request::Resign { force } => self.resign(g, force).await,
            Request::UnlockReset => {
                let _ = std::fs::remove_file(self.cfg.unlock_disabled_flag());
                g.dedup.clear("unlock");
                Ok("已恢复自动解锁".into())
            }
        };
        r.map(Response::text).unwrap_or_else(Response::error)
    }

    fn snapshot(&self, g: &Inner) -> Snapshot {
        let unlock = self.unlock_cfg();
        Snapshot {
            state: g.state.clone(),
            device: g.info.clone(),
            persisted: g.persisted.clone(),
            auto_unlock: unlock.enabled(),
            unlock_disabled: unlock.disabled_by_failure(),
            notify_configured: self.cfg.notify.any(),
            log_tail: self.log.tail(15),
        }
    }

    /// WDA 可用就返回客户端（本进程 runner 存活）
    fn wda<'a>(&self, g: &'a mut Inner) -> Option<&'a mut WdaClient<DeviceTransport>> {
        if g.runner.as_ref().is_some_and(|r| r.alive()) { g.wda.as_mut() } else { None }
    }

    fn require_wda<'a>(&self, g: &'a mut Inner) -> Result<&'a mut WdaClient<DeviceTransport>, String> {
        let msg = wda_down(&g.state);
        self.wda(g).ok_or(msg)
    }

    /// 配置了密码就在锁屏时自动解锁；没配置而手机锁着则报错（与 toolkit 一致）
    async fn ensure_unlocked(&self, g: &mut Inner) -> Result<(), String> {
        let cfg = self.unlock_cfg();
        // 刚锁屏就唤醒，第一下上滑会卡十几秒后报错（真机实测；锁屏后等 3 秒再解锁一次就成）
        if let Some(left) = g.last_lock.and_then(|t| LOCK_SETTLE.checked_sub(t.elapsed())) {
            tokio::time::sleep(left).await;
        }
        let Some(wda) = self.wda(g) else { return Ok(()) };
        match ictl_unlock::ensure_unlocked(wda, &cfg).await {
            Ok(_) => Ok(()),
            Err(UnlockError::WrongPasscode) => {
                self.notify(g, Event::UnlockDisabled);
                Err(format!("{},已推送通知", UnlockError::WrongPasscode))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    async fn screen(&self, g: &mut Inner) -> Result<ScreenSize, String> {
        if let Some(w) = self.wda(g)
            && let Ok(s) = w.screen_size().await
        {
            return Ok(s);
        }
        g.persisted
            .screen
            .or(self.cfg.screen_fallback)
            .ok_or_else(|| "WDA 不可用,也不知道屏幕尺寸;等 WDA 起来一次,或在 .env 里设 SCREEN_W/SCREEN_H".into())
    }

    /// HID 会话（WDA 不可用时的后备路线）。出错时丢掉会话和隧道，下次重建
    async fn hid<'a>(&self, g: &'a mut Inner) -> Result<&'a mut HidSession, String> {
        if g.hid.is_none() {
            let screen = self.screen(g).await?;
            let device = g.device.clone().ok_or("手机未连接")?;
            let t = self.ensure_tunnel(g, &device).await.map_err(|e| format!("HID 后备路线不可用：{e}"))?;
            match t.hid(screen).await {
                Ok(h) => g.hid = Some(h),
                Err(e) => {
                    g.tunnel = None;
                    return Err(format!("HID 后备路线不可用：{e}"));
                }
            }
        }
        Ok(g.hid.as_mut().expect("刚建好"))
    }

    async fn hid_do<F>(&self, g: &mut Inner, f: F) -> Result<(), String>
    where
        F: for<'h> FnOnce(
            &'h mut HidSession,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ictl_device::Result<()>> + Send + 'h>>,
    {
        let r = f(self.hid(g).await?).await;
        g.hid_used = Some(Instant::now());
        if let Err(e) = r {
            close_hid(g).await;
            g.tunnel = None;
            return Err(format!("HID 命令失败: {e}"));
        }
        Ok(())
    }

    async fn status_text(&self, g: &mut Inner) -> String {
        let daemon_line = format!("守护进程: {}", g.state.describe());
        let unlock = self.unlock_cfg();
        let Some(w) = self.wda(g) else {
            return format!(
                "{}\n读界面元素、按名字点击、启动 App、输入中文要等 WDA 恢复\n{daemon_line}",
                wda_down(&g.state)
            );
        };
        let r: Result<String, WdaError> = async {
            let ready = w.ready().await?;
            let app = w.active_app().await?;
            let s = w.screen_size().await?;
            let locked = w.locked().await?;
            let mut lock = if !locked {
                "未锁屏".to_owned()
            } else if unlock.disabled_by_failure() {
                "已锁屏(自动解锁之前失败过已停用,需要人工解锁,核对密码后运行 iphone-control unlock-reset)".to_owned()
            } else if unlock.enabled() {
                "已锁屏(操作时会自动解锁)".to_owned()
            } else {
                "已锁屏(未配置自动解锁,需要人工解锁)".to_owned()
            };
            if !locked && unlock.disabled_by_failure() {
                lock += ";自动解锁已停用";
            }
            let ready = if ready { "True" } else { "False" };
            Ok(format!(
                "WDA 就绪: {ready}\n锁屏: {lock}\n前台 App: {app}\n屏幕: {}x{} 点",
                fmt_g(s.width),
                fmt_g(s.height)
            ))
        }
        .await;
        match r {
            Ok(t) => format!("{t}\n{daemon_line}"),
            Err(e) => format!("{e}\n读界面元素、按名字点击、启动 App、输入中文要等 WDA 恢复\n{daemon_line}"),
        }
    }

    async fn screenshot(&self, g: &mut Inner, region: Option<(f64, f64, f64, f64)>) -> Result<Response, String> {
        self.ensure_unlocked(g).await?;
        let wda_png = match self.wda(g) {
            Some(w) => w.screenshot_png().await.ok(),
            None => None,
        };
        let (png, via) = match wda_png {
            Some(p) => (p, "WDA"),
            None => {
                let device = g.device.clone().ok_or("手机未连接")?;
                let t = self.ensure_tunnel(g, &device).await.map_err(|e| format!("HID 命令失败: {e}"))?;
                match t.screenshot_png().await {
                    Ok(p) => (p, "HID"),
                    Err(e) => {
                        close_hid(g).await;
                        g.tunnel = None;
                        return Err(format!("HID 命令失败: {e}"));
                    }
                }
            }
        };
        let screen = self.screen(g).await?;
        let (jpeg, caption) = match region {
            None => imaging::full_screen(&png, screen, via)?,
            Some(r) => imaging::region(&png, screen, r, via)?,
        };
        Ok(Response::Image { jpeg_base64: base64::engine::general_purpose::STANDARD.encode(jpeg), caption })
    }

    async fn list_elements(&self, g: &mut Inner, filter: &str, limit: usize) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let els = self.require_wda(g)?.elements().await.map_err(|e| e.to_string())?;
        let shown: Vec<&elements::Element> =
            if filter.is_empty() { els.iter().collect() } else { elements::filter(&els, filter) };
        let lines: Vec<_> = shown.iter().take(limit).map(|e| e.describe()).collect();
        let more = if shown.len() > limit {
            format!("\n……另有 {} 个未列出,可用 filter 缩小范围", shown.len() - limit)
        } else {
            String::new()
        };
        Ok(if lines.is_empty() { "没有找到带文字的元素".into() } else { lines.join("\n") } + &more)
    }

    async fn tap(&self, g: &mut Inner, x: f64, y: f64) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let pos = format!("({},{})", fmt_g(x), fmt_g(y));
        if let Some(w) = self.wda(g) {
            match w.tap(x, y).await {
                Ok(()) => return Ok(format!("已点击 {pos}")),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        self.hid_do(g, |h| Box::pin(h.tap(x, y))).await?;
        Ok(format!("已点击 {pos}(经 HID)"))
    }

    async fn tap_element(&self, g: &mut Inner, text: &str, index: usize) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let w = self.require_wda(g)?;
        let els = w.elements().await.map_err(|e| e.to_string())?;
        let matches = elements::find(&els, text);
        if matches.is_empty() {
            return Ok(format!("没找到包含 \"{text}\" 的元素,先用 iphone_list_elements 或截图看看屏幕"));
        }
        if index >= matches.len() {
            let list: Vec<_> = matches.iter().map(|e| e.describe()).collect();
            return Ok(format!("只匹配到 {} 个:\n{}", matches.len(), list.join("\n")));
        }
        let e = matches[index];
        w.tap(e.center.0 as f64, e.center.1 as f64).await.map_err(|e| e.to_string())?;
        let note = if matches.len() > 1 {
            format!("(共匹配 {} 个,点的是第 {index} 个)", matches.len())
        } else {
            String::new()
        };
        Ok(format!("已点击 {}{note}", e.describe()))
    }

    async fn long_press(&self, g: &mut Inner, x: f64, y: f64, seconds: f64) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.touch_and_hold(x, y, seconds).await.map_err(|e| e.to_string())?;
        Ok(format!("已长按 ({},{}) {} 秒", fmt_g(x), fmt_g(y), fmt_g(seconds)))
    }

    async fn swipe(&self, g: &mut Inner, from: (f64, f64), to: (f64, f64), seconds: f64) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let desc = format!("({},{}) → ({},{})", fmt_g(from.0), fmt_g(from.1), fmt_g(to.0), fmt_g(to.1));
        if let Some(w) = self.wda(g) {
            match w.actions(&swipe_steps(from, to, (seconds * 1000.0) as u64)).await {
                Ok(()) => return Ok(format!("已滑动 {desc}")),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        self.hid_do(g, |h| Box::pin(h.drag(from, to, seconds))).await?;
        Ok(format!("已滑动 {desc}(经 HID)"))
    }

    async fn type_text(&self, g: &mut Inner, text: &str, submit: bool) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let payload = if submit { format!("{text}\n") } else { text.to_owned() };
        let n = text.chars().count();
        if let Some(w) = self.wda(g) {
            match w.keys(&payload).await {
                Ok(()) => return Ok(format!("已输入 {n} 个字符{}", if submit { ",并按了回车" } else { "" })),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        if !payload.is_ascii() {
            return Err("WDA 不可用,HID 只能输入英文和符号".into());
        }
        let p = payload.clone();
        self.hid_do(g, move |h| Box::pin(async move { h.type_ascii(&p).await })).await?;
        Ok(format!("已输入 {n} 个字符(经 HID)"))
    }

    async fn press_button(&self, g: &mut Inner, button: &str) -> Result<String, String> {
        if button == "lock" {
            if let Some(w) = self.wda(g) {
                // WDA 锁屏偶尔等 5 秒报 500（手机其实已锁）：确认锁了就不退到 HID（HID 会开屏幕媒体流）
                let locked = match w.lock().await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::info!("WDA 锁屏报错：{e}");
                        w.wda_reports_locked().await.unwrap_or(false)
                    }
                };
                if locked {
                    g.last_lock = Some(Instant::now());
                    return Ok("已锁屏".into());
                }
            }
            self.hid_do(g, |h| Box::pin(h.press(Button::Lock))).await?;
            g.last_lock = Some(Instant::now());
            return Ok("已锁屏".into());
        }
        let (wda_name, hid_button) = match button {
            "home" => ("home", Button::Home),
            "volume_up" => ("volumeUp", Button::VolumeUp),
            "volume_down" => ("volumeDown", Button::VolumeDown),
            _ => return Err("button 只能是 home / volume_up / volume_down / lock".into()),
        };
        self.ensure_unlocked(g).await?;
        if let Some(w) = self.wda(g) {
            match w.press_button(wda_name).await {
                Ok(()) => return Ok(format!("已按 {button}")),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        self.hid_do(g, |h| Box::pin(h.press(hid_button))).await?;
        Ok(format!("已按 {button}"))
    }

    async fn list_apps(&self, g: &mut Inner, include_system: bool) -> Result<Vec<AppInfo>, String> {
        if let Some(a) = g.apps.get(&include_system) {
            return Ok(a.clone());
        }
        let device = g.device.clone().ok_or("手机未连接")?;
        let a = device.list_apps(include_system).await.map_err(|e| e.to_string())?;
        g.apps.insert(include_system, a.clone());
        Ok(a)
    }

    async fn resolve_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        if apps::looks_like_bundle_id(app) {
            return Ok(app.to_owned());
        }
        // 先只查用户 App；列系统 App 要好几秒，只在找不到时才去取
        let user = self.list_apps(g, false).await?;
        if let Some(b) = user.iter().find(|a| a.name == app) {
            return Ok(b.bundle_id.clone());
        }
        let system = self.list_apps(g, true).await?;
        apps::resolve(app, [&user[..], &system[..]])
            .ok_or_else(|| format!("找不到名为 \"{app}\" 的 App,用 iphone_list_apps 查看"))
    }

    async fn launch_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        let bid = self.resolve_app(g, app).await?;
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.launch_app(&bid).await.map_err(|e| e.to_string())?;
        Ok(format!("已启动 {bid}"))
    }

    async fn terminate_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        let bid = self.resolve_app(g, app).await?;
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.terminate_app(&bid).await.map_err(|e| e.to_string())?;
        Ok(format!("已结束 {bid}"))
    }

    async fn open_url(&self, g: &mut Inner, url: &str) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.open_url(url).await.map_err(|e| e.to_string())?;
        Ok(format!("已打开 {url}"))
    }

    // ------------------------------------------------------------------ 诊断

    async fn doctor(&self, g: &mut Inner) -> String {
        let mut out: Vec<String> = Vec::new();
        let mut fails = 0;

        out.push("== 设备连接".into());
        match ictl_device::list_devices().await {
            Ok(_) => out.push("✅ usbmuxd 在运行".into()),
            Err(e) => {
                out.push(format!(
                    "❌ 连不上 usbmuxd：{e}\n     → 容器里由守护进程自动拉起；宿主机部署请启动 usbmuxd 服务"
                ));
                fails += 1;
            }
        }
        let usb = ictl_device::apple_usb_present(Path::new(SYSFS_USB));
        if usb {
            out.push("✅ USB 上有 Apple 设备".into());
        } else {
            out.push("❌ USB 上没有 Apple 设备\n     → 插好数据线;用 USB-C 转接头的话换一根试试".into());
            fails += 1;
        }
        match (&g.device, &g.info) {
            (Some(d), info) => {
                let i = info.clone().unwrap_or_default();
                out.push(format!("✅ 已连接 {} {} iOS {}", d.udid, i.product_type, i.product_version));
                let major = i.major_version();
                if major > 0 && major < 17 {
                    out.push("⚠️  iOS 低于 17  未在此版本测试过".into());
                } else if major > 0 && major < 27 {
                    out.push("⚠️  iOS 低于 27  HID 后备路线(免签名截图/点击)不可用,只能走 WDA".into());
                }
            }
            (None, _) => {
                out.push("❌ 守护进程看不到手机\n     → 等 30 秒,会自动重启 usbmuxd;仍不行就重启容器".into());
                fails += 1;
            }
        }
        out.push(format!("   状态：{}", g.state.describe()));

        out.push("== Apple ID 与签名".into());
        match self.account().selected_email().await {
            Ok(_) => out.push("✅ 已登录 Apple ID".into()),
            Err(_) => {
                out.push("❌ 没有登录 Apple ID\n     → iphone-control login".into());
                fails += 1;
            }
        }
        match (&g.persisted.wda_bundle, g.persisted.profile_left()) {
            (Some(b), Some(left)) => {
                out.push(format!("✅ WDA 已签名 {b}"));
                if left.is_zero() {
                    out.push("❌ 描述文件已过期\n     → iphone-control resign --force".into());
                    fails += 1;
                } else if left < Duration::from_secs(48 * 3600) {
                    out.push(format!("⚠️  描述文件只剩 {} 小时  守护进程会自动重签", left.as_secs() / 3600));
                } else {
                    out.push(format!("✅ 描述文件有效  还剩 {} 天", left.as_secs() / 86400));
                }
            }
            _ => {
                out.push("❌ WDA 还没签名安装\n     → iphone-control sign".into());
                fails += 1;
            }
        }
        if let Some(r) = &g.persisted.last_resign {
            out.push(format!("   上次重签：{r}"));
        }

        out.push("== WDA".into());
        match self.wda(g) {
            Some(w) if w.ready().await.unwrap_or(false) => {
                out.push("✅ WDA 就绪（经 usbmuxd 直连，本机不监听端口）".into())
            }
            _ => {
                out.push(format!("❌ WDA 不可用：{}", g.state.describe()));
                fails += 1;
            }
        }

        out.push("== 附加功能".into());
        if self.cfg.notify.any() {
            out.push("✅ 通知已配置".into());
        } else {
            out.push("⚠️  没有配置通知  重签失败、手机离线、需要输密码时不会提醒你".into());
        }
        let unlock = self.unlock_cfg();
        if unlock.disabled_by_failure() {
            out.push("❌ 自动解锁已停用(之前输错过)\n     → 核对密码后 iphone-control unlock-reset".into());
            fails += 1;
        } else if unlock.enabled() {
            out.push("✅ 自动解锁已启用".into());
        } else {
            out.push("⚠️  自动解锁未启用  手机锁屏时需要人工解锁(可选:iphone-control passcode set)".into());
        }
        out.push(String::new());
        out.push(if fails == 0 { "全部正常".into() } else { format!("{fails} 项需要处理") });
        out.join("\n")
    }

    pub async fn shutdown(&self) {
        let mut g = self.inner.lock().await;
        self.log.log("收到停止信号,退出");
        self.drop_session(&mut g).await;
        if let Some(mut c) = g.usbmuxd.take() {
            let _ = c.kill().await;
        }
    }
}
