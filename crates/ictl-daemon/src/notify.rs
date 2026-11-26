//! 通知：Bark、Telegram。同一事件去重（[`Dedup`]），恢复时推送一次。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::NotifyConfig;

/// Why the previous daemon exited abnormally before this one started
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RestartReason {
    /// The watchdog found no heartbeat progress for `silent_secs` and exited so the service manager restarts it
    Watchdog { silent_secs: u64 },
    /// The previous daemon disappeared without a clean shutdown (crash, OOM, SIGKILL, power loss)
    Unexpected,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Event {
    ResignFailed(String),
    SessionExpired(String),
    NeedsAutomationPin,
    WdaRecovered,
    UnlockDisabled,
    DeviceOffline {
        minutes: u64,
    },
    DeviceOnline {
        minutes: u64,
    },
    /// 签名时新建了证书；`revoked`：团队里证书满了，还吊销了一张旧的
    CertificateChanged {
        revoked: bool,
    },
    /// 上一次守护进程异常退出或由看门狗触发重启，重启后由新进程自感知并告警（带持久化防风暴冷却）
    DaemonRestarted {
        reason: RestartReason,
        /// 近 1 小时内异常重启总次数（含本次）
        recent: u32,
        /// 自上次通知后在冷却期内被合并的异常重启次数
        suppressed: u32,
    },
}

/// Which device a notification is about: its name for the title, and its kind for the words iOS itself shows
/// (the passcode prompt says “iPad passcode” on an iPad)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub kind: &'static str,
}

impl Target {
    /// The device as the daemon knows it; without its details, the device in general
    pub fn of(info: Option<&ictl_device::DeviceInfo>) -> Self {
        let Some(i) = info else {
            return Self { name: ictl_l10n::tr!("设备", "Device").into(), kind: "iPhone" };
        };
        let kind = if i.product_type.starts_with("iPad") {
            "iPad"
        } else if i.product_type.starts_with("iPod") {
            "iPod"
        } else {
            "iPhone"
        };
        let name = i.name.trim();
        Self { name: if name.is_empty() { kind.into() } else { name.into() }, kind }
    }

    /// For `notify-test`
    pub fn sample() -> Self {
        Self { name: "iPhone".into(), kind: "iPhone" }
    }
}

impl Event {
    /// 去重用的键：同一类事件只推一次，直到被对应的恢复事件清掉或过了冷却时间
    fn key(&self) -> &'static str {
        match self {
            Self::ResignFailed(_) => "resign",
            Self::SessionExpired(_) => "session",
            Self::NeedsAutomationPin => "pin",
            Self::WdaRecovered => "pin-recovered",
            Self::UnlockDisabled => "unlock",
            Self::DeviceOffline { .. } => "offline",
            Self::DeviceOnline { .. } => "online",
            Self::CertificateChanged { .. } => "cert",
            Self::DaemonRestarted { .. } => "daemon",
        }
    }

    /// 每类事件一个样例（`iphone-control notify-test` 用），顺序与枚举一致
    pub fn samples() -> Vec<Event> {
        vec![
            Self::ResignFailed(
                ictl_l10n::tr!("（测试）苹果服务器返回错误", "(test) Apple's server returned an error").into(),
            ),
            Self::SessionExpired(ictl_l10n::tr!("（测试）令牌已过期", "(test) the token expired").into()),
            Self::NeedsAutomationPin,
            Self::WdaRecovered,
            Self::UnlockDisabled,
            Self::DeviceOffline { minutes: 10 },
            Self::DeviceOnline { minutes: 12 },
            Self::CertificateChanged { revoked: true },
            Self::DaemonRestarted { reason: RestartReason::Watchdog { silent_secs: 600 }, recent: 1, suppressed: 0 },
        ]
    }

    /// 样例的短名，`notify-test <名字>` 只发这一类
    pub fn sample_name(&self) -> &'static str {
        self.key()
    }

    /// `t`: the device it is about (titles name it; the passcode prompt is called what iOS calls it there)
    pub fn title_body(&self, t: &Target) -> (String, String) {
        let (name, kind) = (&t.name, t.kind);
        match self {
            Self::ResignFailed(why) => (
                ictl_l10n::trf!("{name} WDA 重签失败", "{name}: re-signing WDA failed"),
                ictl_l10n::trf!("{why}\n描述文件过期后 WDA 将无法启动(截图/点击的 HID 路线不受影响)。\n排查: iphone-control doctor", "{why}\nOnce the provisioning profile expires WDA can't start (the HID path for screenshots/taps is not affected).\nTroubleshoot: iphone-control doctor"),
            ),
            Self::SessionExpired(why) => (
                ictl_l10n::tr!("Apple ID 会话已失效", "Apple ID session expired").into(),
                ictl_l10n::trf!("{why}\n下次重签前需要重新登录: iphone-control login", "{why}\nSign in again before the next re-sign: iphone-control login"),
            ),
            Self::NeedsAutomationPin => (
                ictl_l10n::trf!("{name} 需要输入锁屏密码", "{name} needs the passcode"),
                ictl_l10n::trf!("设备重启后要启用 UI 自动化:请解锁 {kind},在「为 XCTest 输入 {kind} 密码」弹窗里输入锁屏密码。弹窗没出现或已关闭的话,1 分钟内会重新弹出。", "After a reboot, UI automation has to be enabled: unlock the {kind} and enter the passcode in the “Enter {kind} passcode for XCTest” prompt. If the prompt didn't appear or was dismissed, it comes back within a minute."),
            ),
            Self::WdaRecovered => (ictl_l10n::trf!("{name} WDA 已恢复", "{name}: WDA is back"), ictl_l10n::tr!("已输入密码,WDA 重新就绪,可以正常操作了。", "Passcode entered, WDA is ready again and everything works.").into()),
            Self::UnlockDisabled => (
                ictl_l10n::trf!("{name} 自动解锁失败,已停用", "{name}: auto-unlock failed and was disabled"),
                ictl_l10n::tr!("整串密码输完手机仍锁着(可能密码配置不对),或输入途中出错、无法确认密码是否已提交。为免累计输错,自动解锁已停用。\n请人工解锁手机、核对密码(iphone-control passcode set),再运行 iphone-control unlock-reset。", "The phone stayed locked after entering the whole passcode (the configured passcode may be wrong), or an error happened while entering it so it's unclear whether it was submitted. Auto-unlock is now disabled so wrong attempts don't pile up.\nUnlock the phone by hand, check the passcode (iphone-control passcode set), then run iphone-control unlock-reset.").into(),
            ),
            Self::DeviceOffline { minutes } => (
                ictl_l10n::trf!("{name} 已离线 {minutes} 分钟", "{name} offline for {minutes} minutes"),
                ictl_l10n::tr!("连不上手机:检查 USB 线、手机是否没电关机。重新连上后会自动恢复。", "Can't reach the phone: check the USB cable and whether the phone ran out of battery. It recovers automatically once reconnected.").into(),
            ),
            Self::DeviceOnline { minutes } => (ictl_l10n::trf!("{name} 已重新连上", "{name} reconnected"), ictl_l10n::trf!("离线约 {minutes} 分钟,正在自动恢复。", "Offline for about {minutes} minutes, recovering automatically.")),
            Self::CertificateChanged { revoked: true } => (
                ictl_l10n::tr!("签名证书换了,旧证书已被吊销", "Signing certificate replaced, an old one was revoked").into(),
                ictl_l10n::tr!("这次签名新建了证书;免费 Apple ID 最多两张,满了就吊销了一张旧的。用旧证书签的 WDA(可能在别的设备,或用同一个 Apple ID 的其他机器、其他侧载工具上)起不来了:这里控制的设备切过去时会自动重签,也可以运行 iphone-control sign。", "This signing created a new certificate; a free Apple ID holds at most two, so an old one was revoked. WDA signed with it (maybe on other devices, or on other machines or sideloading tools using the same Apple ID) won't start any more: devices controlled here are re-signed automatically when you switch to them, or run iphone-control sign.").into(),
            ),
            Self::CertificateChanged { revoked: false } => (
                ictl_l10n::tr!("签名证书换了", "Signing certificate replaced").into(),
                ictl_l10n::tr!("这次签名新建了证书(第一次在这台机器上签名,或原来的证书已被别处吊销)。之前用别的证书签的设备,WDA 起不来时会自动重签,也可以运行 iphone-control sign。", "This signing created a new certificate (the first signing on this machine, or the previous one was revoked elsewhere). Devices signed with another certificate are re-signed automatically when their WDA won't start, or run iphone-control sign.").into(),
            ),
            Self::DaemonRestarted { reason, recent, suppressed } => {
                let extra = if *suppressed > 0 && *recent > *suppressed {
                    ictl_l10n::trf!(
                        "\n冷却期内已合并 {suppressed} 次异常重启（近 1 小时共 {recent} 次）；如持续发生请排查循环崩溃。",
                        "\n{suppressed} earlier abnormal restart(s) were suppressed during cooldown ({recent} in the last hour); check for a crash loop if this keeps happening."
                    )
                } else if *suppressed > 0 {
                    ictl_l10n::trf!(
                        "\n自上次通知起冷却期内已合并 {suppressed} 次异常重启；如持续发生请排查循环崩溃。",
                        "\n{suppressed} earlier abnormal restart(s) were suppressed since the previous alert; check for a crash loop if this keeps happening."
                    )
                } else if *recent > 1 {
                    ictl_l10n::trf!(
                        "\n近 1 小时内已异常重启 {recent} 次。",
                        "\n{recent} abnormal restarts in the last hour."
                    )
                } else {
                    String::new()
                };
                match reason {
                    RestartReason::Watchdog { silent_secs } => (
                        ictl_l10n::tr!("守护进程卡死，已由看门狗重启", "Daemon stuck, restarted by watchdog").into(),
                        ictl_l10n::trf!(
                            "上一个守护进程已 {silent_secs} 秒没有进展，看门狗退出后已由后台服务重新拉起。{extra}\n排查：iphone-control status（或查看 logs/daemon.log）",
                            "The previous daemon made no progress for {silent_secs} s; the watchdog exited and the service restarted it.{extra}\nCheck: iphone-control status (or logs/daemon.log)"
                        ),
                    ),
                    RestartReason::Unexpected => (
                        ictl_l10n::tr!("守护进程异常退出后已重启", "Daemon restarted after unexpected exit").into(),
                        ictl_l10n::trf!(
                            "上一个守护进程未正常关闭就退出了（可能是崩溃、被系统终止或主机断电），现已重新启动。{extra}\n排查：iphone-control status（或查看 logs/daemon.log）",
                            "The previous daemon exited without shutting down cleanly (crash, killed by the OS, or power loss) and has been restarted.{extra}\nCheck: iphone-control status (or logs/daemon.log)"
                        ),
                    ),
                }
            }
        }
    }
}

/// 事件去重：同一键在冷却期内只放行一次；`clear` 后可以再次放行
#[derive(Debug, Default)]
pub struct Dedup {
    sent: HashMap<&'static str, Instant>,
}

/// 冷却：重签失败、会话失效 20 小时内不重复推；异常重启 30 分钟冷却（持久化防风暴）；其余等恢复事件清除
fn cooldown(e: &Event) -> Option<Duration> {
    match e {
        Event::ResignFailed(_) | Event::SessionExpired(_) => Some(Duration::from_secs(20 * 3600)),
        Event::DaemonRestarted { .. } => Some(crate::watchdog::ALERT_COOLDOWN),
        // 证书每换一次都要知道
        Event::WdaRecovered | Event::DeviceOnline { .. } | Event::CertificateChanged { .. } => Some(Duration::ZERO),
        _ => None,
    }
}

impl Dedup {
    pub fn should_send(&mut self, e: &Event, now: Instant) -> bool {
        let key = e.key();
        let ok = match (self.sent.get(key), cooldown(e)) {
            (None, _) => true,
            (Some(_), Some(cd)) if cd.is_zero() => true,
            (Some(t), Some(cd)) => now.duration_since(*t) >= cd,
            (Some(_), None) => false,
        };
        if ok {
            self.sent.insert(key, now);
        }
        ok
    }

    pub fn clear(&mut self, key: &'static str) {
        self.sent.remove(key);
    }

    /// 之前是否推过（用来决定恢复时要不要推「已恢复」）
    pub fn was_sent(&self, key: &'static str) -> bool {
        self.sent.contains_key(key)
    }
}

/// 实际发送，返回成功的渠道数。失败只记日志，不影响守护进程
pub async fn send(cfg: &NotifyConfig, title: &str, body: &str) -> usize {
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(20)).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!("通知客户端创建失败：{e}", "Creating the notification client failed: {e}")
            );
            return 0;
        }
    };
    let mut ok = 0;
    if let (Some(token), Some(chat)) = (&cfg.tg_bot_token, &cfg.tg_chat_id) {
        let r = client
            .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
            .form(&[("chat_id", chat.as_str()), ("text", &format!("{title}\n{body}"))])
            .send()
            .await
            .and_then(|r| r.error_for_status());
        match r {
            Ok(_) => ok += 1,
            Err(e) => tracing::warn!(
                "{}",
                ictl_l10n::log_trf!("Telegram 发送失败：{}", "Sending via Telegram failed: {}", e.without_url())
            ),
        }
    }
    if let Some(key) = &cfg.bark_key {
        let r = client
            .post(format!("{}/{key}", cfg.bark_server.trim_end_matches('/')))
            .form(&[("title", title), ("body", body), ("group", "iPhone")])
            .send()
            .await
            .and_then(|r| r.error_for_status());
        match r {
            Ok(_) => ok += 1,
            Err(e) => tracing::warn!(
                "{}",
                ictl_l10n::log_trf!("Bark 发送失败：{}", "Sending via Bark failed: {}", e.without_url())
            ),
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_cover_every_event_kind_once() {
        let names: Vec<&str> = Event::samples().iter().map(|e| e.sample_name()).collect();
        let mut uniq = names.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), names.len());
        // 新增事件时这里的数字要跟着改，提醒同时补样例
        assert_eq!(names.len(), 9);
    }

    #[test]
    fn same_event_is_sent_once_until_cleared() {
        let mut d = Dedup::default();
        let t = Instant::now();
        assert!(d.should_send(&Event::NeedsAutomationPin, t));
        assert!(!d.should_send(&Event::NeedsAutomationPin, t + Duration::from_secs(3600)));
        d.clear("pin");
        assert!(d.should_send(&Event::NeedsAutomationPin, t));
    }

    #[test]
    fn resign_failure_has_20h_cooldown() {
        let mut d = Dedup::default();
        let t = Instant::now();
        assert!(d.should_send(&Event::ResignFailed("x".into()), t));
        assert!(!d.should_send(&Event::ResignFailed("y".into()), t + Duration::from_secs(19 * 3600)));
        assert!(d.should_send(&Event::ResignFailed("z".into()), t + Duration::from_secs(21 * 3600)));
    }

    #[test]
    fn recovery_events_always_pass() {
        let mut d = Dedup::default();
        let t = Instant::now();
        assert!(d.should_send(&Event::WdaRecovered, t));
        assert!(d.should_send(&Event::WdaRecovered, t));
    }

    #[test]
    fn texts_match_toolkit() {
        let t = Target::sample();
        assert_eq!(Event::NeedsAutomationPin.title_body(&t).0, "iPhone 需要输入锁屏密码");
        assert_eq!(Event::DeviceOffline { minutes: 10 }.title_body(&t).0, "iPhone 已离线 10 分钟");
    }

    /// The device by its name, and iOS's own words for an iPad
    #[test]
    fn a_notification_names_its_device() {
        let info = |name: &str, product_type: &str| ictl_device::DeviceInfo {
            name: name.into(),
            product_type: product_type.into(),
            ..Default::default()
        };
        let ipad = Target::of(Some(&info("客厅的iPad", "iPad11,1")));
        assert_eq!((ipad.name.as_str(), ipad.kind), ("客厅的iPad", "iPad"));
        let (title, body) = ictl_l10n::with_lang(ictl_l10n::Lang::Zh, || Event::NeedsAutomationPin.title_body(&ipad));
        assert_eq!(title, "客厅的iPad 需要输入锁屏密码");
        assert!(body.contains("为 XCTest 输入 iPad 密码") && !body.contains("iPhone"), "{body}");
        assert_eq!(Target::of(Some(&info("", "iPhone16,2"))).name, "iPhone", "no name: its kind");
        let unknown = ictl_l10n::with_lang(ictl_l10n::Lang::En, || Target::of(None));
        assert_eq!(unknown.name, "Device");
        let (title, _) =
            ictl_l10n::with_lang(ictl_l10n::Lang::En, || Event::DeviceOffline { minutes: 3 }.title_body(&ipad));
        assert_eq!(title, "客厅的iPad offline for 3 minutes");
    }

    #[test]
    fn daemon_restarted_messages_render_both_reasons_and_storm_counts() {
        let t = Target::sample();
        let (title, body) = ictl_l10n::with_lang(ictl_l10n::Lang::Zh, || {
            Event::DaemonRestarted { reason: RestartReason::Watchdog { silent_secs: 615 }, recent: 1, suppressed: 0 }
                .title_body(&t)
        });
        assert!(title.contains("看门狗"), "{title}");
        assert!(body.contains("615") && !body.contains("合并") && !body.contains("近 1 小时"), "{body}");

        let (title, body) = ictl_l10n::with_lang(ictl_l10n::Lang::En, || {
            Event::DaemonRestarted { reason: RestartReason::Unexpected, recent: 11, suppressed: 9 }.title_body(&t)
        });
        assert!(title.contains("unexpected exit"), "{title}");
        assert!(body.contains("9 earlier abnormal restart") && body.contains("11 in the last hour"), "{body}");

        let (_, body) = ictl_l10n::with_lang(ictl_l10n::Lang::Zh, || {
            Event::DaemonRestarted { reason: RestartReason::Unexpected, recent: 3, suppressed: 0 }.title_body(&t)
        });
        assert!(body.contains("近 1 小时内已异常重启 3 次"), "{body}");

        // Memory Dedup uses the same 30-minute cooldown as persistent throttling.
        let mut d = Dedup::default();
        let now = Instant::now();
        let ev = Event::DaemonRestarted { reason: RestartReason::Unexpected, recent: 1, suppressed: 0 };
        assert!(d.should_send(&ev, now));
        assert!(!d.should_send(&ev, now + Duration::from_secs(10 * 60)));
        assert!(d.should_send(&ev, now + crate::watchdog::ALERT_COOLDOWN));
    }
}
