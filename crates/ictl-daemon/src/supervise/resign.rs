//! Certificate self-healing, Apple ID session checks, and WDA signing / re-signing.

use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::core::{Daemon, DeviceProfile, Inner};
use crate::machine::DeviceState;
use crate::notify::Event;

/// 整个重签（登录会话、注册设备、签名、上传安装）的时限：苹果服务器或安装卡住时不能永远「正在重签」
const RESIGN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Apple ID 会话检查的时限（容得下 `ictl_sign::AccountDir::session` 最多 3 次尝试 + 退避）
const SESSION_CHECK_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// 会话检查遇到 Anisette / 网络瞬断或超时时的缩短复查间隔：不误发会话失效通知，较快重试确认
pub(crate) const SESSION_CHECK_TRANSIENT_RETRY: Duration = Duration::from_secs(60 * 60);

/// 因为证书自动重签同一台设备的最短间隔：重签也治不好时（例如其实是没信任开发者）不反复签
pub(crate) const CERT_HEAL_EVERY: Duration = Duration::from_secs(6 * 3600);

impl Daemon {
    /// 设备拒绝启动 WDA（只报 unexpected response）时，看是不是证书的问题，是就自动重签这台（见 `ictl_sign` 的模块说明）：
    /// 设备上的 WDA 不是本机的证书签的（别的机器、旧的数据目录签的，签它的证书多半被吊销了），或本机的证书已不在团队里
    /// （别处新建证书时把它吊销了）。都不是就多半是还没信任开发者，留给用户（状态里有提示）。
    /// 同一台 [`CERT_HEAL_EVERY`] 内只自动重签一次
    pub(super) fn maybe_heal_certificate(self: &Arc<Self>, g: &mut Inner) {
        let Some(udid) = g.device.as_ref().map(|d| d.udid.clone()) else { return };
        if g.resigning || g.persisted.wda_bundle.is_none() {
            return;
        }
        if g.cert_heals.get(&udid).is_some_and(|t| t.elapsed() < CERT_HEAL_EVERY) {
            return;
        }
        g.cert_heals.insert(udid.clone(), Instant::now());
        tokio::task::spawn_local(self.clone().heal_certificate(udid, g.dev.foreign_wda));
    }

    async fn heal_certificate(self: Arc<Self>, udid: String, foreign: Option<bool>) {
        let Ok(_lease) = crate::maintenance::shared() else { return };
        if self.account().selected_email().await.is_err() {
            return;
        }
        let why = if foreign == Some(true) {
            ictl_l10n::tr!(
                "这台设备上的 WDA 不是本机的签名证书签的（别的机器或旧的数据目录签的，签它的证书多半已被吊销）",
                "the WDA on this device wasn't signed with this machine's certificate (another machine or an old data directory signed it, and that certificate was most likely revoked)"
            )
        } else {
            match self.check_own_certificate().await {
                Some(false) => ictl_l10n::tr!(
                    "本机的签名证书已不在 Apple ID 的团队里（别处新建证书时把它吊销了）",
                    "this machine's signing certificate is no longer in the Apple ID's team (it was revoked when a certificate was created elsewhere)"
                ),
                // 证书没问题（或查不了）：多半是还没信任开发者
                _ => return,
            }
        };
        {
            let mut g = self.inner.lock().await;
            // 查证书期间切到了别的设备，或用户已经在重签了
            if g.device.as_ref().map(|d| d.udid.as_str()) != Some(udid.as_str()) || g.resigning {
                return;
            }
            self.log.log(&ictl_l10n::log_trf!(
                "WDA 起不来：{why}，自动重签这台设备",
                "WDA won't start: {why}; re-signing this device automatically"
            ));
            self.set_state(
                &mut g,
                DeviceState::WdaStarting {
                    last_error: Some(ictl_l10n::trf!(
                        "{why}。正在自动重签这台设备，约 1–2 分钟，保持设备解锁",
                        "{why}. Re-signing this device automatically, about 1–2 minutes; keep it unlocked"
                    )),
                },
            );
            self.publish(&g);
        }
        if let Err(e) = self.resign_now(true).await {
            let mut g = self.inner.lock().await;
            self.notify(&mut g, Event::ResignFailed(e.to_string()));
        }
    }

    /// 联网查本机的签名证书还在不在团队里，记进 `own_cert`（doctor 显示）。登录不了、查不了返回 None
    async fn check_own_certificate(&self) -> Option<bool> {
        let account = self.account();
        // plume_core 的 future 不是 Send，放到阻塞线程里 block_on（同重签）；也不持锁
        let r = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                tokio::time::timeout(SESSION_CHECK_TIMEOUT, async {
                    account.session().await?.own_certificate_listed().await
                })
                .await
            })
        })
        .await;
        match r {
            Ok(Ok(Ok(Some(listed)))) => {
                self.inner.lock().await.own_cert = Some((Instant::now(), listed));
                Some(listed)
            }
            Ok(Ok(Err(e))) => {
                tracing::warn!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "查本机的签名证书失败：{e}",
                        "Checking this machine's signing certificate failed: {e}"
                    )
                );
                None
            }
            _ => None,
        }
    }

    // ------------------------------------------------------------------ 重签与会话

    /// The Apple ID session checked (and, while it is good, whether this machine's certificate is still in the
    /// team). `lead`, the leader's lock ([`super::session`]), goes into the blocking thread and comes back: a daemon
    /// stopping meanwhile ends without letting go of it before the check is done
    pub(super) async fn check_session_with(&self, lead: std::fs::File) -> (SessionCheckOutcome, Option<std::fs::File>) {
        let account = self.account();
        // plume_core 的 future 不是 Send，放到阻塞线程里 block_on（同重签）；也不持锁
        // 会话好的话顺便查本机的签名证书还在不在团队里（同一次登录）
        let r = tokio::task::spawn_blocking(move || {
            let held = lead;
            let outcome = tokio::runtime::Handle::current().block_on(async move {
                let check = async {
                    let s = account.session().await?;
                    Ok::<_, ictl_sign::SignError>(match s.own_certificate_listed().await {
                        Ok(listed) => listed,
                        // 会话是好的，只是这一步没查成：不算会话失效，记一笔
                        Err(e) => {
                            tracing::warn!(
                                "{}",
                                ictl_l10n::log_trf!(
                                    "查本机的签名证书失败：{e}",
                                    "Checking this machine's signing certificate failed: {e}"
                                )
                            );
                            None
                        }
                    })
                };
                classify_session_check_result(tokio::time::timeout(SESSION_CHECK_TIMEOUT, check).await)
            });
            (outcome, held)
        })
        .await;
        match r {
            Ok((outcome, held)) => (outcome, Some(held)),
            Err(e) => (
                SessionCheckOutcome::Transient(ictl_l10n::log_trf!(
                    "会话检查任务异常：{e}",
                    "Session check task failed: {e}"
                )),
                None,
            ),
        }
    }

    /// What a check found, logged (and notified, when `notify`: the notification was claimed, [`super::session`]).
    /// Returns when the next one is due, and whether this instance still leads
    pub(super) fn apply_session_check(
        &self,
        g: &mut Inner,
        outcome: SessionCheckOutcome,
        now: Instant,
        notify: bool,
    ) -> (Duration, bool) {
        let every = self.cfg.session_check;
        match outcome {
            SessionCheckOutcome::Ok(listed) => {
                g.dedup.clear("session");
                if let Some(l) = listed {
                    g.own_cert = Some((now, l));
                    if !l {
                        self.log.log(ictl_l10n::log_tr!(
                            "本机的签名证书已不在 Apple ID 的团队里（别处新建证书时把它吊销了）：用它签的 WDA 起不来，会自动重签",
                            "This machine's signing certificate is no longer in the Apple ID's team (revoked when a certificate was created elsewhere): WDA signed with it won't start and gets re-signed automatically"
                        ));
                    }
                }
                (every, true)
            }
            SessionCheckOutcome::NotLoggedIn => (every, true),
            SessionCheckOutcome::Transient(e) => {
                let retry = every.min(SESSION_CHECK_TRANSIENT_RETRY);
                let mins = (retry.as_secs() / 60).max(1);
                self.log.log(&ictl_l10n::log_trf!(
                    "Apple ID 会话检查遇到临时网络/Anisette 故障，{mins} 分钟后重试（暂不视为会话失效）：{e}",
                    "Apple ID session check hit a transient network/Anisette failure; retrying in {mins} min (not treating the session as expired): {e}"
                ));
                (retry, true)
            }
            // this instance may not use the session as configured: another one that may takes over
            SessionCheckOutcome::Misconfigured(e) => {
                self.log.log(&ictl_l10n::log_trf!(
                    "Apple ID 会话检查没有执行：{e}",
                    "The Apple ID session check wasn't run: {e}"
                ));
                (every, false)
            }
            SessionCheckOutcome::Expired(e) => {
                self.log.log(&ictl_l10n::log_trf!(
                    "Apple ID 会话检查失败：{e}",
                    "Checking the Apple ID session failed: {e}"
                ));
                if notify {
                    self.notify(g, Event::SessionExpired(e));
                }
                (every, true)
            }
        }
    }

    /// 检查描述文件并按需重签（`force` 立即重签）。分三段：持锁取参数 → 不持锁签名、安装（几十秒）→ 持锁写结果
    pub(crate) async fn resign_now(self: &Arc<Self>, force: bool) -> crate::error::Result<String> {
        let _lease = crate::maintenance::shared()?;
        let (device, name, pass) = {
            let mut g = self.inner.lock().await;
            let g = &mut *g;
            // 看这台手机上装的那份，不看最近一次签名：给别的手机签过之后，这台装的还是更早签的、更早到期
            if !force && g.persisted.wda_bundle.is_some() {
                self.refresh_device_profile(g).await;
                if g.dev.device_profile == DeviceProfile::Missing {
                    // 自动重签不替没装过的手机注册设备（占 Apple ID 的设备名额），交给用户决定
                    return Ok(
                        ictl_l10n::tr!("这台手机上没有装当前 Apple ID 签的 WDA,不自动重签;要装就运行 iphone-control setup", "No WDA signed by the current Apple ID on this phone, not re-signing automatically; run iphone-control setup to install it").into()
                    );
                }
                if let Some(left) = g.dev.device_profile.left(&g.persisted)
                    && left > self.cfg.resign_min
                {
                    return Ok(ictl_l10n::trf!(
                        "描述文件还剩 {} 小时,无需重签",
                        "{} hours left on the provisioning profile, no need to re-sign",
                        left.as_secs() / 3600
                    ));
                }
            }
            if g.resigning {
                return Ok(ictl_l10n::tr!(
                    "正在重签,稍后用 iphone-control status 查看结果",
                    "Re-signing, check the result later with iphone-control status"
                )
                .into());
            }
            let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
            // held until the result is written: a switch doesn't wait for a re-signing, it is refused meanwhile
            let pass = self.gate.admit(Some(crate::admission::Long::Resign))?;
            let name = g
                .info
                .as_ref()
                .map(|i| i.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "iphone-control".into());
            g.resigning = true;
            self.log.log(ictl_l10n::log_tr!("开始重签", "Re-signing started"));
            (device, name, pass)
        };
        let signed_udid = device.udid.clone();
        let account = self.account();
        let (wda_dir, unsigned, signed_path) =
            (self.cfg.wda_dir(), self.cfg.wda_unsigned.clone(), self.cfg.wda_signed());
        let fetch = self.cfg.wda_unsigned_is_cache() && !unsigned.is_file();
        if fetch {
            self.log.log(&ictl_l10n::log_trf!(
                "还没有未签名的 WDA 包，先下载 {}",
                "No unsigned WDA package yet, downloading {} first",
                crate::wda_pkg::WDA_VERSION
            ));
        }
        // plume_core 的签名 future 不是 Send（持有 &mut dyn FnMut 和非 Send 的私钥），放到阻塞线程里 block_on；
        // 错误用 Text 保留双语：日志和落盘按守护进程自己的语言，返回给调用方再按请求语言，不会互相串味
        let r: Result<ictl_sign::Signed, ictl_l10n::Text> = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current()
                .block_on(tokio::time::timeout(RESIGN_TIMEOUT, async move {
                    if fetch {
                        crate::wda_pkg::ensure(&unsigned).await?;
                    }
                    let session = account.session().await.map_err(|e| ictl_l10n::Text::of(&e))?;
                    session.ensure_device(&device.udid, &name).await.map_err(|e| ictl_l10n::Text::of(&e))?;
                    std::fs::create_dir_all(&wda_dir).map_err(|e| ictl_l10n::Text::from(e.to_string()))?;
                    let tmp = wda_dir.join("WDA-signing.ipa");
                    let signed =
                        session.sign_ipa(&unsigned, &tmp, &device.udid).await.map_err(|e| ictl_l10n::Text::of(&e))?;
                    device.install(&tmp).await.map_err(|e| {
                        ictl_l10n::textf!(
                            "安装到手机失败(手机锁屏?)：{e}",
                            "Installing on the phone failed (phone locked?): {e}"
                        )
                    })?;
                    std::fs::rename(&tmp, &signed_path).map_err(|e| ictl_l10n::Text::from(e.to_string()))?;
                    Ok(signed)
                }))
                .unwrap_or_else(|_| {
                    Err(ictl_l10n::textf!(
                        "{} 分钟内没有完成(苹果服务器或安装卡住)",
                        "not finished within {} minutes (Apple's servers or the install got stuck)",
                        RESIGN_TIMEOUT.as_secs() / 60
                    ))
                })
        })
        .await
        .unwrap_or_else(|e| Err(ictl_l10n::textf!("签名任务异常退出：{e}", "The signing task exited abnormally: {e}")));

        let mut g = self.inner.lock().await;
        let g = &mut *g;
        g.resigning = false;
        let r = match r {
            Ok(s) => {
                if s.new_certificate {
                    self.log.log(if s.revoked_certificate {
                        ictl_l10n::log_tr!(
                            "签名新建了证书，团队里的证书满了，吊销了一张旧的：用它签的 WDA 在所有设备上都起不来了",
                            "Signing created a new certificate and revoked an old one (the team was full): WDA signed with it won't start on any device"
                        )
                    } else {
                        ictl_l10n::log_tr!(
                            "签名新建了证书（本机没有私钥，或原来的证书已不在团队里）",
                            "Signing created a new certificate (no local key, or the previous one is no longer in the team)"
                        )
                    });
                    self.notify(g, Event::CertificateChanged { revoked: s.revoked_certificate });
                }
                // 刚用它签成功，本机的证书肯定在团队里
                g.own_cert = Some((Instant::now(), true));
                let exp = s.profile.expires.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                g.persisted.team_id = Some(s.team_id);
                g.persisted.wda_bundle = Some(s.bundle_id);
                g.persisted.profile_expires = Some(exp);
                g.persisted.record_resign(now_secs(), None);
                g.persisted.save(&self.cfg.state_file());
                // 新包装上后旧的 runner 会失效，下一轮巡检重新拉起。签名期间切到了别的设备就不动它的会话
                if pass.current() && g.device.as_ref().is_some_and(|d| d.udid == signed_udid) {
                    // 装新包时旧的后台 WDA 已被结束：忘掉它（进程号可能被别的进程用上），巡检马上重新拉起
                    self.forget_detached_here(g);
                    self.drop_session(g).await;
                    self.refresh_device_profile(g).await;
                    g.backoff.reset();
                    self.wake.notify_one();
                }
                g.dedup.clear("resign");
                let expires = crate::eventlog::format_local(exp);
                // 事件日志按守护进程的语言，返回给调用方的按请求的语言
                self.log.log(&ictl_l10n::log_trf!(
                    "重签完成,新描述文件到期 {expires}",
                    "Re-signing done, the new provisioning profile expires {expires}"
                ));
                Ok(ictl_l10n::trf!(
                    "重签完成,新描述文件到期 {expires}",
                    "Re-signing done, the new provisioning profile expires {expires}"
                ))
            }
            Err(e) => {
                g.persisted.record_resign(now_secs(), Some(ictl_l10n::in_own_lang(|| e.to_string())));
                g.persisted.save(&self.cfg.state_file());
                self.log.log(&ictl_l10n::log_trf!("重签失败：{e}", "Re-signing failed: {e}"));
                Err(e.to_string().into())
            }
        };
        self.publish(g);
        r
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionCheckOutcome {
    Ok(Option<bool>),
    NotLoggedIn,
    Transient(String),
    /// The configuration keeps the session from being used (the anisette state's origin): logged, never reported as
    /// expired, checked again on the usual schedule
    Misconfigured(String),
    Expired(String),
}

impl SessionCheckOutcome {
    /// As kept in the shared state ([`super::session::CheckState::last`]), in the daemon's own language
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Ok(_) => ictl_l10n::log_tr!("会话有效", "session valid").into(),
            Self::NotLoggedIn => ictl_l10n::log_tr!("没登录 Apple ID", "not signed in").into(),
            Self::Transient(e) => ictl_l10n::log_trf!("临时故障：{e}", "transient failure: {e}"),
            Self::Misconfigured(e) => ictl_l10n::log_trf!("没有执行：{e}", "not run: {e}"),
            Self::Expired(e) => ictl_l10n::log_trf!("会话失效：{e}", "session expired: {e}"),
        }
    }
}

fn classify_session_check_result(
    res: std::result::Result<ictl_sign::Result<Option<bool>>, tokio::time::error::Elapsed>,
) -> SessionCheckOutcome {
    match res {
        Ok(Ok(listed)) => SessionCheckOutcome::Ok(listed),
        Ok(Err(ictl_sign::SignError::NotLoggedIn)) => SessionCheckOutcome::NotLoggedIn,
        Ok(Err(e)) if e.is_transient() || matches!(e, ictl_sign::SignError::Io(_)) => {
            SessionCheckOutcome::Transient(ictl_l10n::in_own_lang(|| e.to_string()))
        }
        Ok(Err(e)) if e.is_configuration() => {
            SessionCheckOutcome::Misconfigured(ictl_l10n::in_own_lang(|| e.to_string()))
        }
        Ok(Err(e)) => SessionCheckOutcome::Expired(ictl_l10n::in_own_lang(|| e.to_string())),
        Err(_) => SessionCheckOutcome::Transient(ictl_l10n::log_trf!(
            "{} 分钟内没有完成",
            "not finished within {} minutes",
            SESSION_CHECK_TIMEOUT.as_secs() / 60
        )),
    }
}

pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_check_transient_failure_logs_and_retries_sooner_without_alerting() {
        let dir = std::env::temp_dir().join(format!("ictl-session-check-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let mut cfg = crate::config::Config::from_env(&dir);
        cfg.notify = crate::config::NotifyConfig::default();
        let d = Daemon::new(cfg);

        // Verify classification separates transient / timeout from real session invalidation.
        let transient = classify_session_check_result(Ok(Err(ictl_sign::SignError::Transient("anisette down".into()))));
        assert!(matches!(transient, SessionCheckOutcome::Transient(ref s) if s.contains("anisette down")));

        let timed_out =
            tokio::time::timeout(Duration::ZERO, std::future::pending::<ictl_sign::Result<Option<bool>>>()).await;
        let timeout_outcome = classify_session_check_result(timed_out);
        assert!(matches!(timeout_outcome, SessionCheckOutcome::Transient(_)), "{timeout_outcome:?}");

        let not_logged_in = classify_session_check_result(Ok(Err(ictl_sign::SignError::NotLoggedIn)));
        assert_eq!(not_logged_in, SessionCheckOutcome::NotLoggedIn);

        let expired = classify_session_check_result(Ok(Err(ictl_sign::SignError::SessionInvalid("code 1100".into()))));
        assert!(matches!(expired, SessionCheckOutcome::Expired(ref s) if s.contains("code 1100")));

        // Routine 24h schedule hits a transient failure: no SessionExpired alert, retry pulled in to 1h.
        let now = Instant::now();
        let every = d.cfg.session_check;
        {
            let mut g = d.inner.lock().await;
            assert_eq!(d.apply_session_check(&mut g, transient, now, true), (SESSION_CHECK_TRANSIENT_RETRY, true));
            assert!(!g.dedup.was_sent("session"), "transient glitch must not trigger SessionExpired");
            assert_eq!(
                d.apply_session_check(&mut g, timeout_outcome, now, true),
                (SESSION_CHECK_TRANSIENT_RETRY, true)
            );
            assert!(!g.dedup.was_sent("session"));

            // NotLoggedIn: the usual schedule, no alert
            assert_eq!(d.apply_session_check(&mut g, not_logged_in, now, true), (every, true));
            assert!(!g.dedup.was_sent("session"));

            // a configuration this instance may not use the session with: another one leads
            let mis = SessionCheckOutcome::Misconfigured("origin".into());
            assert_eq!(d.apply_session_check(&mut g, mis, now, true), (every, false));

            // expired, but the notification was claimed by a leader before: logged only
            d.apply_session_check(&mut g, expired.clone(), now, false);
            assert!(!g.dedup.was_sent("session"));
            // Real SessionInvalid does alert via Event::SessionExpired.
            d.apply_session_check(&mut g, expired, now, true);
            assert!(g.dedup.was_sent("session"), "confirmed session invalidation must alert");

            // A subsequent transient error does not wipe the session alert dedup state.
            d.apply_session_check(&mut g, SessionCheckOutcome::Transient("blip".into()), now, true);
            assert!(g.dedup.was_sent("session"));

            // Recovery clears dedup and records own_cert.
            d.apply_session_check(&mut g, SessionCheckOutcome::Ok(Some(true)), now, true);
            assert!(!g.dedup.was_sent("session"));
            assert_eq!(g.own_cert, Some((now, true)));
        }

        let log_text = std::fs::read_to_string(dir.join("logs/daemon.log")).unwrap();
        assert!(log_text.contains("anisette down"), "{log_text}");
        let notify_lines = log_text.lines().filter(|l| l.contains("通知：") || l.contains("Notification:")).count();
        assert_eq!(notify_lines, 1, "only confirmed expiration should dispatch a notification:\n{log_text}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
