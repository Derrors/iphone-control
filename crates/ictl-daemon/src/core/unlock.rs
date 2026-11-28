//! Auto-unlock as the daemon keeps it: each device's passcode and its record of being disabled after a failure
//! (`ictl_unlock::DisableStore`), the passcode an older version saved for no device, and `unlock-reset`

use super::*;

/// Stands for the UDID while no device is controlled: it has no passcode, and only an older version's flag disables it
pub(super) const NO_DEVICE: &str = "_none";

/// At start-up, before any device is unlocked: brings an older version's single flag, or a change a crash cut short,
/// into line with the per-device records ([`ictl_unlock::DisableStore::migrate`])
pub(super) fn migrate_unlock_disabled(cfg: &Config, log: &EventLog) {
    match cfg.unlock_disabled().migrate() {
        Ok(ictl_unlock::Migration::None) => {}
        Ok(ictl_unlock::Migration::Legacy) => log.log(ictl_l10n::log_tr!(
            "旧版本停用了自动解锁（不知道是哪台设备），现在每台设备都停用，用 unlock-reset 逐台恢复",
            "An older version disabled auto-unlock without saying for which device: it is now disabled for every device; re-enable each with unlock-reset"
        )),
        Ok(ictl_unlock::Migration::Orphan(udid)) => log.log(&ictl_l10n::log_trf!(
            "上次改停用标记时中断了，保守起见 {} 的自动解锁保持停用",
            "Changing the disabled record was cut short last time: auto-unlock stays disabled for {} to be safe",
            udid.as_deref().unwrap_or(ictl_l10n::tr!("所有设备", "every device"))
        )),
        Ok(ictl_unlock::Migration::Shadow) => {
            tracing::info!("auto-unlock disabled records had no shadow flag; written again")
        }
        // nothing is trusted until it works: every device counts as disabled (`DisableStore::settled`), no record is
        // changed, and the next change (or start) tries again
        Err(e) => log.log(&ictl_l10n::log_trf!(
            "整理自动解锁停用记录失败，所有设备暂时停用自动解锁（修好后重启守护进程或运行 unlock-reset）：{e}",
            "Tidying the auto-unlock disabled records failed, so auto-unlock is off for every device for now (restart the daemon or run unlock-reset once fixed): {e}"
        )),
    }
}

impl Daemon {
    /// 自动解锁配置：用设备 `udid` 自己的密码。没接设备时密码文件不存在（自动解锁不可用）
    pub(crate) fn unlock_cfg(&self, udid: Option<&str>) -> UnlockConfig {
        let udid = udid.unwrap_or(NO_DEVICE);
        UnlockConfig {
            passcode_file: self.cfg.passcode_file_for(udid),
            disabled: self.cfg.unlock_disabled().device(udid),
            timing: Default::default(),
        }
    }

    /// `unlock-reset`: re-enables auto-unlock for `udid`, or for the device being controlled. Runs under the device
    /// lock, so this daemon isn't entering a passcode meanwhile; only that device's record goes
    /// `unlock-reset` as a request: a path of its own, not the device lane's. Not queued behind what holds the device,
    /// which may be entering a passcode (a reset waiting for it would clear the record a wrong entry just wrote), and
    /// nothing of the device lane is done on the way (no WDA restart, no cache cleared): it only changes files
    /// `expect`: the caller's [`crate::core::CallCtx::expect_udid`], checked against the device actually re-enabled
    /// (this path skips the device lane, where it is checked otherwise)
    pub(super) fn unlock_reset_now(&self, req: &Request, expect: Option<&str>) -> Response {
        let udid = match req {
            Request::UnlockResetDevice { udid } => udid.as_deref(),
            _ => None,
        };
        let Ok(g) = self.inner.try_lock() else {
            return Response::error(ictl_l10n::tr!(
                "守护进程正在操作这台设备（可能正在输入锁屏密码），没有恢复自动解锁；稍后再运行 unlock-reset",
                "The daemon is busy with the device (it may be entering the passcode), so auto-unlock wasn't re-enabled; run unlock-reset again in a moment"
            ))
            .with_failure(Delivery::NotSent, FailureStage::Preflight);
        };
        match self.unlock_reset(&g, udid, expect) {
            Ok(text) => Response::text_with(text, crate::protocol::Outcome::default()),
            Err(e) => Response::error(e.to_string()),
        }
    }

    pub(crate) fn unlock_reset(
        &self,
        g: &Inner,
        udid: Option<&str>,
        expect: Option<&str>,
    ) -> crate::error::Result<String> {
        let named = match udid {
            // a UDID of a device connected now; not a name (a name no longer the one in the daemon's list would pick
            // another device), and not a device that isn't there (its UDID mistyped, or the wrong one)
            Some(q) => match g.connected.iter().find(|d| d.udid.eq_ignore_ascii_case(q.trim())) {
                Some(d) => Some(d.udid.clone()),
                None => {
                    return Err(ictl_l10n::trf!(
                        "没有接着 UDID 是 {q} 的设备（--device 只收 UDID，用 iphone-control devices 查看）",
                        "No device with the UDID {q} is connected (--device takes a UDID only; see iphone-control devices)"
                    )
                    .into());
                }
            },
            None => None,
        };
        let target = named.or_else(|| g.device.as_ref().map(|d| d.udid.clone()));
        if let (Some(want), Some(t)) = (expect, target.as_deref())
            && !want.eq_ignore_ascii_case(t)
        {
            return Err(ictl_l10n::trf!(
                "要恢复的是 {t}，和请求要求的设备 {want} 不一致，没有恢复",
                "The device to re-enable is {t}, not {want} as the request expected; nothing was re-enabled"
            )
            .into());
        }
        let udid = match target {
            Some(u) => u,
            None => {
                return Err(ictl_l10n::tr!(
                    "没在控制哪台设备，不知道要恢复哪台的自动解锁：用 --device 指定",
                    "No device is being controlled, so it isn't clear which one to re-enable auto-unlock for: pass --device"
                )
                .into());
            }
        };
        let label = self
            .connected_infos(g)
            .iter()
            .find(|i| i.udid.eq_ignore_ascii_case(&udid))
            .map(device_label)
            .unwrap_or_else(|| udid.clone());
        let store = self.cfg.unlock_disabled();
        let r = store
            .reset(&udid)
            .map_err(|e| ictl_l10n::trf!("恢复自动解锁失败：{e}", "Re-enabling auto-unlock failed: {e}"))?;
        let mut text = match r {
            ictl_unlock::Reset::Cleared => {
                self.log.log(&ictl_l10n::log_trf!("已恢复 {udid} 的自动解锁", "Auto-unlock re-enabled for {udid}"));
                ictl_l10n::trf!("已恢复 {label} 的自动解锁", "Auto-unlock re-enabled for {label}")
            }
            ictl_unlock::Reset::WasEnabled => {
                ictl_l10n::trf!("{label} 的自动解锁没有停用", "Auto-unlock wasn't disabled for {label}")
            }
        };
        let retired = self.cfg.retire_legacy_unlock_disabled().unwrap_or_else(|e| {
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!(
                    "撤掉旧版本的停用标记时出错（可能没撤掉，也可能撤了一部分；没撤掉的照旧生效）：{e}",
                    "Removing the older version's flag failed (it may not be removed, or only in part; what is left still applies): {e}"
                )
            );
            false
        });
        if retired {
            self.log.log(ictl_l10n::log_tr!(
                "存了锁屏密码的设备都已解除旧版本的停用，撤掉了旧版本留下的全局停用标记；各设备自己的停用记录仍保留",
                "Every device with a saved passcode is released from the older version's flag, so that global flag is removed; each device's own disabled record stays"
            ));
            text += ictl_l10n::tr!(
                "；已撤掉旧版本留下的全局停用标记（各设备自己的停用记录仍保留）",
                "; the global flag an older version left is removed (each device's own disabled record stays)"
            );
        } else if store.legacy() {
            text += ictl_l10n::tr!(
                "；旧版本留下的停用标记还在，其他设备在各自运行 unlock-reset 之前仍是停用的",
                "; the flag an older version left is still there: other devices stay disabled until unlock-reset is run for each"
            );
        }
        Ok(text)
    }

    /// 旧版本那份不知道是哪台的密码：USB 上只接着一台、正好是要控制的这台、这台还没有自己的密码时，归到它名下。
    /// 接着多台时不动它（也不用它）。上次归到这台时中断了的，不管接着几台都由这台接着做完（`Config::adopt_legacy_passcode`）
    pub(crate) fn adopt_legacy_passcode(&self, g: &Inner) {
        let Some(d) = &g.device else { return };
        // set aside for this device by an adoption cut short: it finishes, whatever else is connected
        let staged = self.cfg.legacy_passcode_staged_for(&d.udid);
        let alone = matches!(&g.connected[..], [only] if only.udid == d.udid);
        if !staged && !(alone && self.cfg.legacy_passcode_file().is_file()) {
            return;
        }
        match self.cfg.adopt_legacy_passcode(&d.udid) {
            // set for this device meanwhile: that one stays, and the old one isn't used while it has its own
            Ok(false) => {}
            Ok(true) => self.log.log(&ictl_l10n::log_trf!("旧的锁屏密码只有一份，USB 上只接着 {}，归到它名下", "There is a single passcode saved by an older version and only {} is on USB; assigning it to that device", d.udid)),
            Err(e) => tracing::warn!("{}", ictl_l10n::log_trf!("把旧的锁屏密码归到 {} 名下失败：{e}", "Assigning the old passcode to {} failed: {e}", d.udid)),
        }
    }
}
