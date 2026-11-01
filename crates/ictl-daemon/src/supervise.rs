//! 巡检和后台任务：usbmuxd 看护、每 10 秒推进状态机、后台启动 WDA、重签、Apple ID 会话检查。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use ictl_device::{Device, DeviceError, Tunnel};
use ictl_wda::{WdaClient, WdaError, WdaRunner, http::DeviceTransport};

use crate::core::{Daemon, Inner, close_hid};
use crate::machine::DeviceState;
use crate::notify::Event;

/// 正常 2～10 秒就绪。手机重启后未授权 automation 时，testmanagerd 握手每次卡 30 秒、要重试一两次，
/// 60 秒常常等不到 runner 启动，也就认不出「需要输入密码」
const WDA_START_TIMEOUT: Duration = Duration::from_secs(120);
/// HID 会话空闲这么久就关掉。它持有屏幕媒体流，开着时 iOS 当作屏幕被远程查看：不会自动锁屏，
/// 还会弹「viewed remotely」通知。重开约 0.5 秒
pub(crate) const HID_IDLE: Duration = Duration::from_secs(30);
pub(crate) const SYSFS_USB: &str = "/sys/bus/usb/devices";
/// usbmuxd 起来后这么久内就退出，算「起不来」，按退避重试
const USBMUXD_QUICK_EXIT: Duration = Duration::from_secs(30);
const USBMUXD_MAX_DELAY: Duration = Duration::from_secs(300);
/// 连续这么多轮列不出设备才当作离线
const LIST_ERRORS_OFFLINE: u32 = 3;
/// 开发者镜像连续挂载失败这么多次就清缓存重新下载
const DDI_FAILURES_INVALIDATE: u32 = 3;

/// 后台启动 WDA 的结果
pub(crate) enum StartOutcome {
    Ready { runner: WdaRunner, client: WdaClient<DeviceTransport>, mounted: Option<String> },
    NeedsUnlock,
    Ddi(String),
    Wda(WdaError),
}

impl Daemon {
    // ------------------------------------------------------------------ usbmuxd

    pub(crate) async fn ensure_usbmuxd(&self, g: &mut Inner, restart: bool) {
        if !self.cfg.manage_usbmuxd {
            return;
        }
        let exited = match g.usbmuxd.as_mut().map(|c| c.try_wait()) {
            Some(Ok(None)) => None,
            Some(Ok(Some(st))) => Some(st.to_string()),
            Some(Err(e)) => Some(e.to_string()),
            None => Some(String::new()),
        };
        if exited.is_none() && !restart {
            return;
        }
        if !restart {
            // 起来就退出（例如宿主机上已有 usbmuxd 占着 socket）：退避重试，不要每 10 秒拆一次会话、写一行日志
            if let Some(why) = exited.filter(|w| !w.is_empty())
                && g.usbmuxd_started.is_some_and(|t| t.elapsed() < USBMUXD_QUICK_EXIT)
            {
                g.usbmuxd_delay = (g.usbmuxd_delay * 2).clamp(Duration::from_secs(10), USBMUXD_MAX_DELAY);
                g.usbmuxd_retry_at = Some(Instant::now() + g.usbmuxd_delay);
                g.usbmuxd_started = None;
                self.log.log(&format!(
                    "usbmuxd 启动后很快就退出了（{why}），{} 秒后再试；宿主机已在运行 usbmuxd 时设 ICTL_MANAGE_USBMUXD=0",
                    g.usbmuxd_delay.as_secs()
                ));
            }
            if g.usbmuxd_retry_at.is_some_and(|t| Instant::now() < t) {
                return;
            }
        }
        // 经旧 usbmuxd 建立的连接（隧道、runner、WDA）都要失效：先停 HID 媒体流（还要经它），再停它
        self.drop_session(g).await;
        if let Some(mut c) = g.usbmuxd.take() {
            let _ = c.kill().await;
        }
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
                g.usbmuxd_started = Some(Instant::now());
                if g.usbmuxd_retry_at.take().is_none() {
                    g.usbmuxd_delay = Duration::ZERO;
                }
                self.log.log("usbmuxd 已(重新)启动");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => self.log.log(&format!("启动 usbmuxd 失败：{e}")),
        }
    }

    // ------------------------------------------------------------------ 状态机

    /// 每 10 秒一次。有请求在用设备就跳过这一轮（巡检不排队，下一轮再来）
    pub async fn tick(self: Arc<Self>) {
        let Ok(mut g) = self.inner.try_lock() else { return };
        let g = &mut *g;
        self.ensure_usbmuxd(g, false).await;

        if g.tunnel.as_ref().is_some_and(Tunnel::hid_open) && g.hid_used.is_none_or(|t| t.elapsed() >= HID_IDLE) {
            close_hid(g).await;
            tracing::info!("HID 会话空闲，已关闭（停止屏幕媒体流）");
        }
        self.poll_start(g).await;

        let devices = match ictl_device::list_devices().await {
            Ok(d) => {
                g.list_errors = 0;
                d
            }
            Err(e) => {
                // usbmuxd 抖一下就当离线会拆掉整个会话：连续几轮都失败才算
                g.list_errors += 1;
                tracing::debug!("列设备失败（第 {} 次）：{e}", g.list_errors);
                if g.list_errors < LIST_ERRORS_OFFLINE {
                    self.publish(g);
                    return;
                }
                Vec::new()
            }
        };
        let target = match &self.cfg.udid {
            Some(u) => devices.into_iter().find(|d| &d.udid == u),
            None => devices.into_iter().next(),
        };
        // 只在容器里（自己看护 usbmuxd、没有 udev）需要；用系统的 usbmuxd 服务时插拔由 udev 处理，也重启不了它
        let usb_present = ictl_device::apple_usb_present(Path::new(SYSFS_USB));
        if self.cfg.manage_usbmuxd && g.mux.observe(usb_present, target.is_some()) {
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
            if g.device.is_some() && g.persisted.wda_bundle.is_some() {
                let d = self.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = d.resign_now(false).await {
                        let mut g = d.inner.lock().await;
                        d.notify(&mut g, Event::ResignFailed(e));
                    }
                });
            }
        }
        if now >= g.next_session_check {
            g.next_session_check = now + self.cfg.session_check;
            tokio::task::spawn_local(self.clone().check_session());
        }
        self.publish(g);
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

        match device.paired().await {
            Ok(true) => {}
            Ok(false) => return self.set_state(g, DeviceState::NeedsTrust),
            // usbmuxd 出错：这一轮不下结论
            Err(e) => return tracing::debug!("查配对记录失败：{e}"),
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
        if g.starting.is_some() || !g.backoff.ready(Instant::now()) {
            return;
        }
        self.spawn_start(g, device, bundle);
    }

    /// 在后台启动 WDA：挂开发者镜像（单独开一条隧道，用完关掉）→ 启动 runner 等就绪。
    /// 最长约 2 分钟（等「Enable UI Automation」密码时），期间不持锁：工具调用照常走 HID 后备路线
    fn spawn_start(&self, g: &mut Inner, device: Device, bundle: String) {
        // 正在等密码时保持这个状态，免得每次重试都在两个状态间来回切、刷日志
        if g.state != DeviceState::NeedsAutomationPin {
            self.set_state(g, DeviceState::WdaStarting { last_error: None });
        }
        self.log.log("启动 WDA");
        let (cache, url) = (self.cfg.ddi_cache(), self.cfg.ddi_url.clone());
        g.starting = Some(tokio::task::spawn_local(async move {
            // 手机重启后开发者镜像可能掉了；已挂载时只是一次查询
            let ddi: Result<Option<String>, DeviceError> = async {
                let mut t = device.open_tunnel().await?;
                let r = async {
                    if t.ddi_mounted().await?.is_some() {
                        return Ok(None);
                    }
                    let dir = ictl_device::ddi::ensure_cache(&cache, &url).await?;
                    Ok(Some(t.mount_ddi(&dir).await?))
                }
                .await;
                t.close().await;
                r
            }
            .await;
            let mounted = match ddi {
                Ok(v) => v,
                Err(DeviceError::PasswordProtected) => return StartOutcome::NeedsUnlock,
                Err(e) => return StartOutcome::Ddi(e.to_string()),
            };
            match WdaRunner::start(device.provider(), &bundle, WDA_START_TIMEOUT).await {
                Ok((runner, client)) => StartOutcome::Ready { runner, client, mounted },
                Err(e) => StartOutcome::Wda(e),
            }
        }));
    }

    /// 后台启动做完了就收结果
    async fn poll_start(&self, g: &mut Inner) {
        if !g.starting.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        let h = g.starting.take().expect("刚检查过");
        let outcome =
            h.await.unwrap_or_else(|e| StartOutcome::Wda(WdaError::StartFailed(format!("启动任务异常：{e}"))));
        match outcome {
            StartOutcome::Ready { runner, client, mounted } => {
                if let Some(v) = mounted {
                    self.log.log(&format!("开发者镜像已挂载 {v}"));
                }
                g.ddi_failures = 0;
                g.runner = Some(runner);
                g.wda = Some(client);
                // 状态在下面 online() 的健康检查里变成 Ready
            }
            StartOutcome::NeedsUnlock => self.set_state(g, DeviceState::NeedsUnlock),
            StartOutcome::Ddi(e) => {
                g.ddi_failures += 1;
                if g.ddi_failures >= DDI_FAILURES_INVALIDATE {
                    self.log.log(&format!("开发者镜像连续 {} 次挂载失败，清掉缓存，下次重新下载", g.ddi_failures));
                    ictl_device::ddi::invalidate(&self.cfg.ddi_cache());
                    g.ddi_failures = 0;
                }
                g.backoff.failed(Instant::now());
                self.set_state(g, DeviceState::WdaStarting { last_error: Some(format!("开发者镜像：{e}")) });
            }
            StartOutcome::Wda(WdaError::NeedsAutomationPin) => {
                g.backoff.waiting_for_pin(Instant::now());
                self.set_state(g, DeviceState::NeedsAutomationPin);
                self.notify(g, Event::NeedsAutomationPin);
            }
            StartOutcome::Wda(e) => {
                g.backoff.failed(Instant::now());
                self.log.log(&format!("WDA 启动失败：{e}"));
                self.set_state(g, DeviceState::WdaStarting { last_error: Some(e.to_string()) });
            }
        }
    }

    // ------------------------------------------------------------------ 重签与会话

    async fn check_session(self: Arc<Self>) {
        let account = self.account();
        // plume_core 的 future 不是 Send，放到阻塞线程里 block_on（同重签）；也不持锁
        let r: Result<(), Option<String>> = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                match account.session().await {
                    Ok(_) => Ok(()),
                    Err(ictl_sign::SignError::NotLoggedIn) => Err(None),
                    Err(e) => Err(Some(e.to_string())),
                }
            })
        })
        .await
        .unwrap_or_else(|e| Err(Some(format!("会话检查任务异常：{e}"))));
        let mut g = self.inner.lock().await;
        match r {
            Ok(()) => g.dedup.clear("session"),
            Err(None) => {}
            Err(Some(e)) => {
                self.log.log(&format!("Apple ID 会话检查失败：{e}"));
                self.notify(&mut g, Event::SessionExpired(e));
            }
        }
    }

    /// 检查描述文件并按需重签（`force` 立即重签）。分三段：持锁取参数 → 不持锁签名、安装（几十秒）→ 持锁写结果
    pub(crate) async fn resign_now(self: &Arc<Self>, force: bool) -> Result<String, String> {
        let (device, name) = {
            let mut g = self.inner.lock().await;
            if !force
                && g.persisted.wda_bundle.is_some()
                && let Some(left) = g.persisted.profile_left()
                && left > self.cfg.resign_min
            {
                return Ok(format!("描述文件还剩 {} 小时,无需重签", left.as_secs() / 3600));
            }
            if g.resigning {
                return Ok("正在重签,稍后用 iphone-control status 查看结果".into());
            }
            let device = g.device.clone().ok_or("手机未连接")?;
            let name = g
                .info
                .as_ref()
                .map(|i| i.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "iphone-control".into());
            g.resigning = true;
            self.log.log("开始重签");
            (device, name)
        };
        let account = self.account();
        let (wda_dir, unsigned, signed_path) =
            (self.cfg.wda_dir(), self.cfg.wda_unsigned.clone(), self.cfg.wda_signed());
        let fetch = self.cfg.wda_unsigned_is_cache() && !unsigned.is_file();
        if fetch {
            self.log.log(&format!("没有未签名的 WDA 包（不在镜像里运行），先下载 {}", crate::wda_pkg::WDA_VERSION));
        }
        // plume_core 的签名 future 不是 Send（持有 &mut dyn FnMut 和非 Send 的私钥），放到阻塞线程里 block_on
        let r: Result<ictl_sign::Signed, String> = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                if fetch {
                    crate::wda_pkg::ensure(&unsigned).await?;
                }
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

        let mut g = self.inner.lock().await;
        let g = &mut *g;
        g.resigning = false;
        let r = match r {
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
        };
        self.publish(g);
        r
    }
}
