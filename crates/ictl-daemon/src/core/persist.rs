//! 存到 `state.json` 的状态（不含秘密），以及 WDA 签名描述文件还剩多久

use super::*;

/// 持久化到 `state.json` 的状态（不含秘密）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persisted {
    pub team_id: Option<String>,
    pub wda_bundle: Option<String>,
    /// 描述文件到期时间（Unix 秒）
    pub profile_expires: Option<u64>,
    /// 设备竖放时的屏幕尺寸（WDA 不可用时 HID 换算坐标用；旧版本可能存的是横屏尺寸，用的时候都先转成竖放）
    pub screen: Option<ScreenSize>,
    /// `screen` 是哪台设备的；换了设备就不能拿来给 HID 换算坐标。也是上次就绪的那台（自动选设备时先接着用它）
    pub screen_udid: Option<String>,
    /// Every device's screen size, by UDID: `screen` only holds the last one, so after switching devices and back
    /// the HID fallback didn't know the first one's size until its WDA came up again ([`Self::screen_for`])
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub screens: std::collections::BTreeMap<String, ScreenSize>,
    /// 上次重签的结果，旧格式：拼好的中文整句。照旧写，旧版本（降级、旧 CLI）还读它；新版本显示时用
    /// [`Self::resign_result`]
    pub last_resign: Option<String>,
    /// 上次重签的结果，按看的人的语言显示（[`Self::last_resign_text`]）。旧版本写的 state.json 没有它
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resign_result: Option<ResignRecord>,
    /// 运行中选定要控制的设备（`iphone-control select`、`iphone_select_device`）；None 是自动选。
    /// 设了 `IPHONE_UDID` 时以它为准，这个不起作用
    pub selected_udid: Option<String>,
    /// 后台启动的 WDA：最近记下的那个后台启动的 WDA（哪台设备上的哪个进程）。只留给旧版本读
    /// （降级后它据此接上或结束这一个）；新版本看 [`Self::detached_wdas`]，经 [`Self::detached_for`] 等读写
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_wda: Option<DetachedWda>,
    /// 每台设备上后台启动的 WDA 的进程号，按 UDID。守护进程重启、切换设备再切回来时
    /// 据此直接接上，不重新启动；改走原生控制、`service stop`/`uninstall` 时据此结束它（不然它一直
    /// 占着 8100 端口，新 runner 的 HTTP 服务起不来）。见 [`crate::detached`]
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub detached_wdas: std::collections::BTreeMap<String, u64>,
    /// The background WDAs launched listening on the phone's loopback only (`USE_IP`, see
    /// `ictl_wda::runner::WDA_BIND_IP`), by UDID. One in [`Self::detached_wdas`] but not here was launched by an older
    /// version, listening on every interface (on Wi-Fi anyone on the network could drive the phone): it isn't attached
    /// to again but launched anew ([`Self::adoptable_for`]). Older versions don't know this field; a WDA they launch
    /// gets a new pid, so it doesn't match here either
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub loopback_wdas: std::collections::BTreeMap<String, u64>,
    /// Which way Home and locking work on each device, by UDID ([`Self::ways_for`]). Kept here, not only in memory:
    /// after a restart or switching devices the first Home on an iPad mini 5 tried the way that doesn't work there,
    /// waited 2 s for the home screen, then pressed again (4.4 s instead of 0.8)
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub button_ways: std::collections::BTreeMap<String, ButtonWays>,
}

/// Which way the buttons work on a device, learned when one way didn't and the other did. Unknown is `false` both
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonWays {
    /// Home through XCTest first: IOHID's didn't get to the home screen and XCTest's did (IOHID's Home hardly ever works
    /// on an iPhone 18 Pro Max / iOS 27.2.1, nor from an app on an iPad mini 5 / iPadOS 26.7.3; an iPhone 12 mini is the
    /// other way round). See `tools::keys::press_home`
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub home_via_xctest: bool,
    /// Lock with WDA's `/wda/lock` first: the power key through IOHID didn't lock and WDA did. On an iPhone 15 Pro Max
    /// (iOS 27.0.1) the power key doesn't work (the call hangs about 5 s, then 3 s more before falling back, about 10 s
    /// a lock), `/wda/lock` takes about 2 s; an iPhone 18 Pro Max (iOS 27.2.1) is the other way round. See
    /// `tools::keys::lock_order`
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lock_via_wda: bool,
}

/// 后台启动的 WDA 进程
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachedWda {
    pub udid: String,
    pub pid: u64,
}

/// 一次重签的结果
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResignRecord {
    /// 结束时间（Unix 秒）
    pub at: u64,
    /// 失败原因；成功时为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Persisted {
    /// The recorded screen size of `udid` (`None`: no device): its own entry, else `screen` if it is this device's
    /// (or from an older version that didn't say whose)
    pub fn screen_for(&self, udid: Option<&str>) -> Option<ScreenSize> {
        if let Some(u) = udid
            && let Some(s) = self.screens.iter().find(|(k, _)| k.eq_ignore_ascii_case(u)).map(|(_, s)| *s)
        {
            return Some(s);
        }
        let same = match (&self.screen_udid, udid) {
            (Some(s), Some(u)) => s.eq_ignore_ascii_case(u),
            (Some(_), None) => false,
            (None, _) => true,
        };
        self.screen.filter(|_| same)
    }

    /// Records `udid`'s screen size; whether anything changed (then save)
    pub fn record_screen(&mut self, udid: &str, size: ScreenSize) -> bool {
        let changed = self.screen != Some(size)
            || self.screen_udid.as_deref() != Some(udid)
            || self.screens.get(udid) != Some(&size);
        self.screen = Some(size);
        self.screen_udid = Some(udid.to_owned());
        self.screens.insert(udid.to_owned(), size);
        changed
    }

    /// How the buttons work on `udid` (`None`: no device), as learned so far
    pub fn ways_for(&self, udid: Option<&str>) -> ButtonWays {
        udid.and_then(|u| self.button_ways.iter().find(|(k, _)| k.eq_ignore_ascii_case(u)).map(|(_, w)| *w))
            .unwrap_or_default()
    }

    /// Changes what is known of `udid`'s buttons with `f`; whether anything changed (then save)
    pub fn record_ways(&mut self, udid: &str, f: impl FnOnce(&mut ButtonWays)) -> bool {
        let key =
            self.button_ways.keys().find(|k| k.eq_ignore_ascii_case(udid)).cloned().unwrap_or_else(|| udid.to_owned());
        let before = self.button_ways.get(&key).copied().unwrap_or_default();
        let mut now = before;
        f(&mut now);
        if now == before {
            return false;
        }
        self.button_ways.insert(key, now);
        true
    }

    /// The WDA launched in the background on `udid`, if one is recorded (by an older version: only the last one)
    pub fn detached_for(&self, udid: &str) -> Option<u64> {
        self.detached_wdas
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(udid))
            .map(|(_, p)| *p)
            .or_else(|| self.detached_wda.as_ref().filter(|d| d.udid.eq_ignore_ascii_case(udid)).map(|d| d.pid))
    }

    /// The background WDA on `udid` that may be attached to again: one this version launched, listening on the
    /// phone's loopback only. Any other is launched anew, which ends it
    pub fn adoptable_for(&self, udid: &str) -> Option<u64> {
        let pid = self.detached_for(udid)?;
        self.loopback_wdas.iter().any(|(k, p)| k.eq_ignore_ascii_case(udid) && *p == pid).then_some(pid)
    }

    /// Every recorded background WDA: (UDID, pid)
    pub fn detached_all(&self) -> Vec<(String, u64)> {
        let mut all: Vec<_> = self.detached_wdas.iter().map(|(u, p)| (u.clone(), *p)).collect();
        if let Some(d) = &self.detached_wda
            && !all.iter().any(|(u, _)| u.eq_ignore_ascii_case(&d.udid))
        {
            all.push((d.udid.clone(), d.pid));
        }
        all
    }

    /// Records the WDA launched in the background on `udid` (the other devices' stay); whether anything changed
    /// (then save)
    pub fn record_detached(&mut self, udid: &str, pid: u64) -> bool {
        // an older version's only record is overwritten below: keep it among the devices' records first
        if let Some(d) = self.detached_wda.clone()
            && !self.detached_wdas.keys().any(|k| k.eq_ignore_ascii_case(&d.udid))
        {
            self.detached_wdas.insert(d.udid, d.pid);
        }
        let key = self
            .detached_wdas
            .keys()
            .find(|k| k.eq_ignore_ascii_case(udid))
            .cloned()
            .unwrap_or_else(|| udid.to_owned());
        let last = Some(DetachedWda { udid: key.clone(), pid });
        // this version launches and attaches to loopback-only WDAs alone: whatever is recorded now is one
        let others = self.loopback_wdas.len();
        self.loopback_wdas.retain(|k, _| !k.eq_ignore_ascii_case(udid) || *k == key);
        let loopback_changed =
            self.loopback_wdas.len() != others || self.loopback_wdas.insert(key.clone(), pid) != Some(pid);
        let changed = loopback_changed || self.detached_wdas.get(&key) != Some(&pid) || self.detached_wda != last;
        self.detached_wdas.insert(key, pid);
        self.detached_wda = last;
        changed
    }

    /// Forgets the background WDA recorded for `udid` (only if it is `pid`, when given); whether anything changed
    /// (then save)
    pub fn forget_detached(&mut self, udid: &str, pid: Option<u64>) -> bool {
        let before = (self.detached_wdas.len(), self.detached_wda.is_some());
        self.detached_wdas.retain(|k, p| !(k.eq_ignore_ascii_case(udid) && pid.is_none_or(|x| x == *p)));
        self.loopback_wdas.retain(|k, p| !(k.eq_ignore_ascii_case(udid) && pid.is_none_or(|x| x == *p)));
        if self
            .detached_wda
            .as_ref()
            .is_some_and(|d| d.udid.eq_ignore_ascii_case(udid) && pid.is_none_or(|x| x == d.pid))
        {
            self.detached_wda = None;
        }
        before != (self.detached_wdas.len(), self.detached_wda.is_some())
    }

    /// 记下这次重签的结果：新字段，加上旧版本认的中文整句（这句照旧用 UTC，和旧版本写的一样）
    pub fn record_resign(&mut self, at: u64, error: Option<String>) {
        let when = crate::eventlog::format_utc(at);
        self.last_resign = Some(match &error {
            None => format!("{when} 成功"),
            Some(e) => format!("{when} 失败：{e}"),
        });
        self.resign_result = Some(ResignRecord { at, error });
    }

    /// 上次重签的结果，按当前语言；只有旧格式时原样给出
    pub fn last_resign_text(&self) -> Option<String> {
        let Some(r) = &self.resign_result else { return self.last_resign.clone() };
        let when = crate::eventlog::format_local(r.at);
        Some(match &r.error {
            None => ictl_l10n::trf!("{when} 成功", "{when} succeeded"),
            Some(e) => ictl_l10n::trf!("{when} 失败：{e}", "{when} failed: {e}"),
        })
    }

    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else { return Self::default() };
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            // 损坏时按空状态启动（会显示「WDA 还没签名」），但要留痕
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!(
                    "state.json 解析失败，按空状态启动：{e}",
                    "Parsing state.json failed, starting with an empty state: {e}"
                )
            );
            Self::default()
        })
    }

    pub(crate) fn save(&self, path: &Path) {
        use std::io::Write;
        let tmp = path.with_extension("json.tmp");
        // nothing written when it doesn't serialize: an empty file in its place would lose the whole state
        let r = serde_json::to_vec_pretty(self).map_err(std::io::Error::other).and_then(|data| {
            // sync before the rename, or a power cut can leave state.json empty
            std::fs::File::create(&tmp)
                .and_then(|mut f| {
                    f.write_all(&data)?;
                    f.sync_all()
                })
                .and_then(|_| std::fs::rename(&tmp, path))
        });
        if let Err(e) = r {
            tracing::warn!("{}", ictl_l10n::log_trf!("保存 state.json 失败：{e}", "Saving state.json failed: {e}"));
        }
    }

    /// 描述文件还剩多久；不知道返回 None
    pub fn profile_left(&self) -> Option<Duration> {
        self.profile_expires.and_then(left_until)
    }
}

/// 离 Unix 时间 `at`（秒）还有多久，过了是 0
fn left_until(at: u64) -> Option<Duration> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(at.saturating_sub(now)))
}

/// 这台手机上装着的 WDA 描述文件（经 misagent 读）。[`Persisted`] 只记最近一次签名：给别的手机签过之后，
/// 它的到期时间不是这台手机上那份的（这台装的还是更早签的），按它判断会该重签时不重签
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum DeviceProfile {
    /// 还没读（手机没接、刚接上还没配对）
    #[default]
    NotChecked,
    /// 读不出来：按签名记录算
    Unknown(String),
    /// 手机上没有当前 Apple ID 给 WDA 签的、包含这台手机的描述文件：没装过 WDA，或装的是别的账号签的
    Missing,
    /// 到期时间（Unix 秒）
    Expires(u64),
}

impl DeviceProfile {
    /// 还剩多久：读到了用手机上的；还没读、读不出来按签名记录；手机上没有返回 None
    pub fn left(&self, persisted: &Persisted) -> Option<Duration> {
        match self {
            Self::Expires(e) => left_until(*e),
            Self::Missing => None,
            Self::NotChecked | Self::Unknown(_) => persisted.profile_left(),
        }
    }
}

/// 从手机上的描述文件（原始内容）里挑 WDA 的：app-id 是「团队 ID.WDA Bundle ID」（不知道团队 ID 时只看 Bundle ID）、
/// 设备列表里有这台手机，取最晚到期的那份。装过几次会留下几份，旧的过期了也还在
pub fn wda_profile(profiles: &[Vec<u8>], team: Option<&str>, bundle: &str, udid: &str) -> Option<ictl_sign::Profile> {
    profiles
        .iter()
        .filter_map(|p| ictl_sign::Profile::parse(p))
        .filter(|p| {
            p.application_identifier
                .split_once('.')
                .is_some_and(|(t, b)| b == bundle && team.is_none_or(|team| t == team))
                && p.devices.iter().any(|d| d.eq_ignore_ascii_case(udid))
        })
        .max_by_key(|p| p.expires)
}

pub fn wda_profile_expiry(profiles: &[Vec<u8>], team: Option<&str>, bundle: &str, udid: &str) -> Option<SystemTime> {
    wda_profile(profiles, team, bundle, udid).map(|p| p.expires)
}
