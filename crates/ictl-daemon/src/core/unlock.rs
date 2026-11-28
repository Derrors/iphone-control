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
        // nothing changed: the records still say what they said, and an unreadable one disables
        Err(e) => tracing::warn!(
            "{}",
            ictl_l10n::log_trf!("整理自动解锁停用标记失败：{e}", "Tidying the auto-unlock disabled records failed: {e}")
        ),
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
    pub(crate) fn unlock_reset(&self, g: &Inner, udid: Option<&str>) -> crate::error::Result<String> {
        let udid = match udid.or(g.device.as_ref().map(|d| d.udid.as_str())) {
            Some(u) => u.to_owned(),
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
        if store.legacy() {
            text += ictl_l10n::tr!(
                "；旧版本留下的停用标记还在，其他设备在各自运行 unlock-reset 之前仍是停用的",
                "; the flag an older version left is still there: other devices stay disabled until unlock-reset is run for each"
            );
        }
        Ok(text)
    }

    /// 旧版本那份不知道是哪台的密码：USB 上只接着一台、正好是要控制的这台、这台还没有自己的密码时，归到它名下。
    /// 接着多台时不动它（也不用它）
    pub(crate) fn adopt_legacy_passcode(&self, g: &Inner) {
        let legacy = self.cfg.legacy_passcode_file();
        let ([only], Some(d)) = (&g.connected[..], &g.device) else { return };
        if only.udid != d.udid || !legacy.is_file() {
            return;
        }
        if self.cfg.passcode_file_for(&d.udid).exists() {
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
