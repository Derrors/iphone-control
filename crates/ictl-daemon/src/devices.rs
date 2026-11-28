//! 接着多台设备时：列出来、选一台控制。
//!
//! 同一时间只控制一台（一个 WDA 会话、一把设备锁）。选哪台见 [`crate::machine::pick_device`]：
//! 选定了就只认那一台；没选定时正在控制的那台一直用到它断开。切换时拆掉上一台的会话，在新的一台上拉起 WDA

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ictl_device::{Device, DeviceInfo, UsbDevice};

use crate::core::{Daemon, Inner, device_label};

/// 按 UDID 或设备名找设备，返回下标：先比 UDID（不分大小写），再比设备名，最后是设备名包含这段文字且只有一台
pub fn resolve_device(devices: &[DeviceInfo], query: &str) -> Result<usize, String> {
    let q = query.trim();
    let listed = || devices.iter().map(device_label).collect::<Vec<_>>().join(ictl_l10n::tr!("、", ", "));
    if devices.is_empty() {
        return Err(ictl_l10n::tr!("USB 上没有接着设备", "No device connected over USB").into());
    }
    if let Some(i) = devices.iter().position(|d| d.udid.eq_ignore_ascii_case(q)) {
        return Ok(i);
    }
    let by_name: Vec<usize> = (0..devices.len()).filter(|&i| devices[i].name == q).collect();
    let hits = if by_name.is_empty() {
        let lq = q.to_lowercase();
        (0..devices.len()).filter(|&i| !lq.is_empty() && devices[i].name.to_lowercase().contains(&lq)).collect()
    } else {
        by_name
    };
    match hits[..] {
        [i] => Ok(i),
        [] => Err(ictl_l10n::trf!("没有接着「{q}」,接着的有 {}", "“{q}” is not connected; connected are {}", listed())),
        _ => Err(ictl_l10n::trf!(
            "「{q}」对上了不止一台:{};用 UDID 指定",
            "“{q}” matches more than one device: {}; use the UDID",
            hits.iter().map(|&i| device_label(&devices[i])).collect::<Vec<_>>().join(ictl_l10n::tr!("、", ", "))
        )),
    }
}

const METADATA_RETRY: Duration = Duration::from_secs(60);
/// Other devices' details are read in the background, this many at a time, each within [`PROBE_BUDGET`]
/// (docs/dev/multi-instance-plan.md 4.1)
pub(crate) const PROBES_AT_ONCE: usize = 2;
const PROBE_BUDGET: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct CachedInfo {
    pub value: Option<DeviceInfo>,
    retry_at: Option<Instant>,
}

impl CachedInfo {
    /// Whether the details are to be read now: incomplete, and not tried within [`METADATA_RETRY`]. A read due is
    /// counted as tried
    fn due(&mut self, now: Instant) -> bool {
        if self.retry_at.is_some_and(|retry| now < retry) {
            return false;
        }
        if self
            .value
            .as_ref()
            .is_some_and(|i| !i.name.is_empty() && !i.product_type.is_empty() && !i.product_version.is_empty())
        {
            return false;
        }
        self.retry_at = Some(now + METADATA_RETRY);
        true
    }

    /// Takes what a read gave, keeping the fields it left empty from before
    fn take(&mut self, mut info: DeviceInfo) {
        if let Some(cached) = &self.value {
            for (field, previous) in [
                (&mut info.product_type, &cached.product_type),
                (&mut info.name, &cached.name),
                (&mut info.product_version, &cached.product_version),
            ] {
                if field.is_empty() {
                    field.clone_from(previous);
                }
            }
        }
        self.value = Some(info);
    }
}

#[cfg(test)]
impl CachedInfo {
    /// [`Self::due`], then the read's details [`taken`](Self::take)
    async fn refresh(&mut self, now: Instant, probe: impl Future<Output = Option<DeviceInfo>>) {
        if self.due(now)
            && let Some(info) = probe.await
        {
            self.take(info);
        }
    }
}

/// Details read in the background, by UDID and the usbmuxd connection they were read over ([`Daemon::probed`])
pub(crate) type Probed = std::sync::Arc<std::sync::Mutex<HashMap<(String, u32), DeviceInfo>>>;

/// Reads a device other than the one controlled, for display (#243)
struct Aside {
    slot: Probed,
    probes: std::sync::Arc<tokio::sync::Semaphore>,
    dir: std::path::PathBuf,
    me: String,
}

impl Aside {
    /// Under a probe's lease, within [`PROBE_BUDGET`]; a device another instance controls is described by its
    /// lease's hint instead
    async fn read(self, d: UsbDevice) {
        let Ok(_permit) = self.probes.acquire_owned().await else { return };
        use crate::lease::{Claim, Holder};
        let info = match crate::lease::try_claim(&self.dir, &d.udid, Holder::probe(&self.me)) {
            Ok(Claim::Got(_probe)) => {
                tokio::time::timeout(PROBE_BUDGET, Device::from_usb(&d).info()).await.ok().and_then(Result::ok)
            }
            Ok(Claim::Held(Some(h))) if h.instance_name().is_some() => {
                Some(DeviceInfo { udid: d.udid.clone(), name: h.name, product_type: h.model, product_version: h.ios })
            }
            _ => None,
        };
        if let Some(info) = info {
            self.slot.lock().unwrap_or_else(|p| p.into_inner()).insert((d.udid, d.device_id), info);
        }
    }
}

impl Daemon {
    /// Read identity on attachment; lockdown can return partial metadata until the device is trusted.
    /// The device whose lease this instance holds is read here, under the device lock: its details are used at once
    /// (`online`). The others are read in the background and taken at the next round, each under a short lease of
    /// its own (`Aside`), so a device another instance controls is never read
    pub(crate) async fn note_connected(&self, g: &mut Inner, devices: Vec<UsbDevice>) {
        g.infos.retain(|udid, _| devices.iter().any(|d| d.udid == *udid));
        let probed = std::mem::take(&mut *self.probed.lock().unwrap_or_else(|p| p.into_inner()));
        let now = Instant::now();
        let mine = g.lease.as_ref().map(|l| l.udid().to_owned());
        let mut here = Vec::new();
        for d in &devices {
            if !g.connected.contains(d) {
                g.infos.remove(&d.udid);
            }
            let cached = g.infos.entry(d.udid.clone()).or_default();
            // read over this very connection: one from before a replug may be another device's state
            if let Some(info) = probed.get(&(d.udid.clone(), d.device_id)) {
                cached.take(info.clone());
            }
            if !cached.due(now) {
                continue;
            }
            if mine.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(&d.udid)) {
                here.push(d.clone());
            } else {
                let aside = Aside {
                    slot: self.probed.clone(),
                    probes: self.probes.clone(),
                    dir: self.cfg.devices_dir(),
                    me: self.cfg.instance_name().to_owned(),
                };
                tokio::task::spawn_local(aside.read(d.clone()));
            }
        }
        // the ones read here, at once rather than one after another
        let mut reads = tokio::task::JoinSet::new();
        for d in here {
            reads.spawn_local(async move { (d.udid.clone(), Device::from_usb(&d).info().await.ok()) });
        }
        while let Some(done) = reads.join_next().await {
            if let Ok((udid, Some(info))) = done
                && let Some(cached) = g.infos.get_mut(&udid)
            {
                cached.take(info);
            }
        }
        g.connected = devices;
    }

    /// The details of the device whose lease was just taken, read now when there are none yet: `online` needs them
    /// at once (the iOS version decides how it is driven)
    /// The iOS version is read now, whatever the retry delay says: reading the device aside (before its lease was
    /// taken) may have spent it without getting there
    pub(crate) async fn read_leased(&self, g: &mut Inner, d: &UsbDevice) {
        let cached = g.infos.entry(d.udid.clone()).or_default();
        if cached.value.as_ref().is_some_and(|i| !i.product_version.is_empty()) {
            return;
        }
        if let Ok(info) = Device::from_usb(d).info().await
            && let Some(cached) = g.infos.get_mut(&d.udid)
        {
            cached.take(info);
        }
    }

    /// The instance controlling `udid`, when it isn't this one (its lease's hint)
    pub(crate) fn controlled_elsewhere(&self, g: &Inner, udid: &str) -> Option<String> {
        if g.lease.as_ref().is_some_and(|l| l.is_for(udid)) {
            return None;
        }
        crate::lease::holder(&self.cfg.devices_dir(), udid).and_then(|h| h.instance_name().map(str::to_owned))
    }

    pub(crate) fn list_devices_text(&self, g: &Inner) -> String {
        let infos = self.connected_infos(g);
        if infos.is_empty() {
            return match self.pinned(g) {
                Some(p) => {
                    ictl_l10n::trf!("USB 上没有接着设备(选定的是 {p})", "No device connected over USB (selected: {p})")
                }
                None => ictl_l10n::tr!("USB 上没有接着设备", "No device connected over USB").into(),
            };
        }
        let current = g.device.as_ref().map(|d| d.udid.as_str());
        let mode = match (&self.cfg.udid, self.pinned(g)) {
            (Some(u), _) => ictl_l10n::trf!("IPHONE_UDID 固定为 {u}", "IPHONE_UDID pins {u}"),
            (None, Some(p)) => ictl_l10n::trf!("选定了 {p}", "{p} selected"),
            (None, None) => ictl_l10n::tr!(
                "自动选(正在控制的那台断开才换)",
                "automatic (switches only when the controlled one disconnects)"
            )
            .into(),
        };
        let mut lines = vec![ictl_l10n::trf!("接着 {} 台,{mode}:", "{} connected, {mode}:", infos.len())];
        for i in &infos {
            let mark = if Some(i.udid.as_str()) == current {
                ictl_l10n::tr!("→ 正在控制 ", "→ controlled ")
            } else {
                "  "
            };
            let pin = if self.cfg.passcode_file_for(&i.udid).is_file() {
                ictl_l10n::tr!("已设", "set")
            } else {
                ictl_l10n::tr!("没设", "not set")
            };
            let by = self
                .controlled_elsewhere(g, &i.udid)
                .map(|n| ictl_l10n::trf!("（由实例 {n} 控制）", " (controlled by the instance {n})"))
                .unwrap_or_default();
            lines.push(ictl_l10n::trf!(
                "{mark}{} 自动解锁密码{pin}{by}",
                "{mark}{} auto-unlock passcode {pin}{by}",
                device_label(i)
            ));
        }
        if self.cfg.udid.is_none() && infos.len() > 1 {
            lines.push(
                ictl_l10n::tr!(
                    "切换:iphone-control select 或 iphone_select_device 工具(UDID 或设备名),auto 改回自动选",
                    "Switch: iphone-control select or the iphone_select_device tool (UDID or device name), auto to go back to automatic"
                )
                .into(),
            );
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(udid: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            udid: udid.into(),
            name: name.into(),
            product_type: "iPhone16,2".into(),
            product_version: "27.0".into(),
        }
    }

    #[tokio::test]
    async fn incomplete_metadata_retries_without_polling_lockdown_each_tick() {
        let start = Instant::now();
        let mut cached = CachedInfo::default();
        cached.refresh(start, async { None }).await;
        cached
            .refresh(start + Duration::from_secs(10), async { panic!("retried a failed query on the next tick") })
            .await;
        let mut partial = dev("A", "Tablet");
        partial.product_version.clear();
        cached.refresh(start + METADATA_RETRY, async { Some(partial) }).await;
        cached
            .refresh(start + METADATA_RETRY + Duration::from_secs(10), async {
                panic!("retried partial metadata on the next tick")
            })
            .await;
        cached.refresh(start + 2 * METADATA_RETRY, async { Some(dev("A", "")) }).await;
        let info = cached.value.as_ref().unwrap();
        assert_eq!((&*info.name, &*info.product_version), ("Tablet", "27.0"));
        cached.refresh(start + 3 * METADATA_RETRY, async { panic!("retried complete metadata") }).await;
    }

    #[test]
    fn passcode_file_is_per_device_and_stays_in_its_dir() {
        let cfg = crate::Config::from_env(std::path::Path::new("/data"));
        assert_eq!(
            cfg.passcode_file_for("00008020-000925043499002E"),
            cfg.passcodes_dir().join("00008020-000925043499002E")
        );
        let odd = cfg.passcode_file_for("../../etc/x");
        assert_eq!(odd.parent(), Some(cfg.passcodes_dir().as_path()));
        assert_ne!(cfg.passcode_file_for("A"), cfg.legacy_passcode_file());
    }

    #[test]
    fn device_is_found_by_udid_then_name() {
        let ds =
            [dev("00008130-AAAA", "Derrors 的 iPhone"), dev("00008020-BBBB", "Derrors 的 iPad"), dev("C", "iPad 2")];
        assert_eq!(resolve_device(&ds, "00008020-bbbb"), Ok(1));
        assert_eq!(resolve_device(&ds, "Derrors 的 iPhone"), Ok(0));
        assert_eq!(resolve_device(&ds, " iphone "), Ok(0));
        // 「iPad」两台都包含：要用 UDID 指定
        assert!(resolve_device(&ds, "ipad").unwrap_err().contains("不止一台"));
        assert!(resolve_device(&ds, "Mac").unwrap_err().contains("没有接着"));
        assert!(resolve_device(&[], "x").is_err());
        // 名字完全相同的优先于包含
        assert_eq!(resolve_device(&ds, "iPad 2"), Ok(2));
    }
}
