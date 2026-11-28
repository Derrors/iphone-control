//! 守护进程配置：全部来自环境变量（服务从 ~/.config/iphone-control/env 读入，手动运行时由 shell 给），都有默认值。秘密只放文件，不放环境变量以外的地方。

use std::path::{Path, PathBuf};
use std::time::Duration;

use ictl_device::ScreenSize;

/// The anchor and registry of this host's instances. An anchor that can't be read is checked again, as an error,
/// before a daemon starts ([`Config::check_instance`])
#[cfg(not(test))]
fn host_registry() -> (Option<crate::instance::Anchor>, PathBuf) {
    let anchor = crate::instance::anchor().ok().flatten();
    let registry = anchor.as_ref().map(|a| a.registry.clone());
    (anchor, registry.unwrap_or_else(|| crate::instance::config_dir().join("instances")))
}

/// Unit tests never see the host's instances
#[cfg(test)]
fn host_registry() -> (Option<crate::instance::Anchor>, PathBuf) {
    (None, std::env::temp_dir().join(format!("ictl-test-no-registry-{}", std::process::id())))
}

#[derive(Debug, Clone)]
pub struct Config {
    /// This instance's own data directory (state, tasks, logs, WDA, developer disk images)
    pub data_dir: PathBuf,
    /// What every instance on this host shares (the Apple ID account, passcodes, auto-unlock's disabled records,
    /// device leases, background-WDA records): the default instance's data directory ([`crate::instance`]). The same
    /// as [`Self::data_dir`] for the default instance
    pub root: PathBuf,
    /// The named instance this is, `None` for the default one ([`crate::instance`])
    pub instance: Option<String>,
    /// This instance's id ([`crate::instance::DEFAULT_ID`] for the default one): a name can be used again, an id isn't
    pub instance_id: String,
    /// The instance registry directory
    pub registry: PathBuf,
    pub socket: PathBuf,
    /// 固定要控制的手机（`IPHONE_UDID`）；None 时可以在运行中选（见 [`crate::core::Persisted::selected_udid`]）。
    /// 都没选是自动选：正在控制的那台一直用到断开，没有正在控制的才取 usbmuxd 列出的第一台
    pub udid: Option<String>,
    /// 未签名的 WDA：默认是数据目录里的 `wda_unsigned_cache`，第一次签名前下载
    pub wda_unsigned: PathBuf,
    pub ddi_url: String,
    /// 自己从 Xcode 提取的 DDI Restore 目录；设了就不下载，也从不删除里面的文件
    pub ddi_dir: Option<PathBuf>,
    pub anisette_url: Option<String>,
    /// WDA 从没连上过时，HID 换算坐标用的屏幕尺寸
    pub screen_fallback: Option<ScreenSize>,
    pub resign_min: Duration,
    pub resign_check: Duration,
    pub session_check: Duration,
    /// 0 表示不提醒
    pub offline_alert: Duration,
    pub notify: NotifyConfig,
    /// iOS 27 及以上怎么控制（`ICTL_IOS_BACKEND`）
    pub ios_backend: IosBackend,
}

/// iOS 27 及以上的手机怎么控制（`ICTL_IOS_BACKEND`）。iOS 26 及以下没有 HID，总是 WDA
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IosBackend {
    /// 原生（默认）
    #[default]
    Auto,
    Native,
    Wda,
}

impl IosBackend {
    pub fn parse(v: Option<&str>) -> Self {
        match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("wda") => Self::Wda,
            Some("native") => Self::Native,
            _ => Self::Auto,
        }
    }

    /// Whether a phone is driven natively: iOS 27 and later (HID needs `dtuhidd`), unless `ICTL_IOS_BACKEND=wda`
    pub fn drives_natively(self, info: Option<&ictl_device::DeviceInfo>) -> bool {
        self != Self::Wda && info.is_some_and(|i| i.major_version() >= 27)
    }
}

#[derive(Clone, Default)]
pub struct NotifyConfig {
    pub tg_bot_token: Option<String>,
    pub tg_chat_id: Option<String>,
    pub bark_server: String,
    pub bark_key: Option<String>,
}

/// Written out (a `{:?}` of the [`Config`] in a log or a panic) without the keys: only whether each is set
impl std::fmt::Debug for NotifyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |v: &Option<String>| if v.is_some() { "<set>" } else { "<unset>" };
        f.debug_struct("NotifyConfig")
            .field("tg_bot_token", &set(&self.tg_bot_token))
            .field("tg_chat_id", &set(&self.tg_chat_id))
            .field("bark_server", &self.bark_server)
            .field("bark_key", &set(&self.bark_key))
            .finish()
    }
}

impl NotifyConfig {
    pub fn any(&self) -> bool {
        (self.tg_bot_token.is_some() && self.tg_chat_id.is_some()) || self.bark_key.is_some()
    }
}

/// 读环境变量，去掉首尾空白，空值当作没设（配置文件里常留着 `KEY=`）
pub fn env(k: &str) -> Option<String> {
    crate::protocol::env::get(k)
}

/// Where to look for ffmpeg (the viewer's H.264 picture; on macOS only the fallback for the native VideoToolbox
/// encoder), in order: `ICTL_FFMPEG`, else `ffmpeg` on `PATH` and, on macOS, Homebrew's places (a launchd service
/// doesn't have them on its `PATH`)
pub fn ffmpeg_paths() -> Vec<PathBuf> {
    if let Some(p) = env(crate::protocol::env::FFMPEG) {
        return vec![PathBuf::from(p)];
    }
    let mut v = vec![PathBuf::from("ffmpeg")];
    if cfg!(target_os = "macos") {
        v.extend(["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"].map(PathBuf::from));
    }
    v
}

fn wda_unsigned_cache(data_dir: &Path) -> PathBuf {
    data_dir.join("wda/WDA-unsigned.ipa")
}

/// The legacy passcode moved aside for `name` (a passcode file's name): bound to that device, never another
const LEGACY_STAGED: &str = ".legacy-for-";

/// A UDID as a passcode file's name: letters, digits and dashes, anything else replaced (never a path)
fn passcode_name(udid: &str) -> String {
    udid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

fn env_num<T: std::str::FromStr>(k: &str, default: T) -> T {
    env(k).and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Config {
    pub fn from_env(data_dir: &Path) -> Self {
        let screen_fallback = match (
            env(crate::protocol::env::SCREEN_W).and_then(|v| v.parse().ok()),
            env(crate::protocol::env::SCREEN_H).and_then(|v| v.parse().ok()),
        ) {
            (Some(width), Some(height)) => Some(ScreenSize { width, height }),
            _ => None,
        };
        let instance = env(crate::protocol::env::INSTANCE).filter(|n| crate::instance::valid_name(n));
        let (anchor, registry) = host_registry();
        // a named instance's data directory is `<root>/instances/<name>`: the root is the anchor's, or that
        let root = match (&instance, &anchor) {
            (None, _) => data_dir.to_owned(),
            (Some(_), Some(a)) => a.root.clone(),
            (Some(_), None) => data_dir.parent().and_then(Path::parent).unwrap_or(data_dir).to_owned(),
        };
        Self {
            data_dir: data_dir.to_owned(),
            root,
            instance_id: if instance.is_some() {
                env(crate::protocol::env::INSTANCE_ID).unwrap_or_default()
            } else {
                crate::instance::DEFAULT_ID.into()
            },
            instance,
            registry,
            // 实际值由命令行给（--socket / ICTL_SOCKET，见 apps/iphone-control）
            socket: crate::protocol::default_socket(),
            udid: env(crate::protocol::env::UDID),
            wda_unsigned: env(crate::protocol::env::WDA_UNSIGNED)
                .map(PathBuf::from)
                .unwrap_or_else(|| wda_unsigned_cache(data_dir)),
            ddi_url: env(crate::protocol::env::DDI_URL).unwrap_or_else(|| ictl_device::ddi::DEFAULT_BASE_URL.into()),
            ddi_dir: env(crate::protocol::env::DDI_DIR).map(PathBuf::from),
            anisette_url: env(crate::protocol::env::ANISETTE_URL),
            screen_fallback,
            resign_min: Duration::from_secs(env_num(crate::protocol::env::RESIGN_MIN_DAYS, 2u64) * 86400),
            resign_check: Duration::from_secs(env_num(crate::protocol::env::RESIGN_CHECK_HOURS, 6u64) * 3600),
            session_check: Duration::from_secs(24 * 3600),
            offline_alert: Duration::from_secs(env_num(crate::protocol::env::OFFLINE_ALERT_MIN, 10u64) * 60),
            notify: NotifyConfig {
                tg_bot_token: env(crate::protocol::env::TG_BOT_TOKEN),
                tg_chat_id: env(crate::protocol::env::TG_CHAT_ID),
                bark_server: env(crate::protocol::env::BARK_SERVER).unwrap_or_else(|| "https://api.day.app".into()),
                bark_key: env(crate::protocol::env::BARK_KEY),
            },
            ios_backend: IosBackend::parse(env(crate::protocol::env::IOS_BACKEND).as_deref()),
        }
    }

    pub fn logs(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
    /// The instance's name as shown and logged: `default` for the default one
    pub fn instance_name(&self) -> &str {
        self.instance.as_deref().unwrap_or(crate::instance::DEFAULT)
    }
    /// Shared by every instance ([`Self::root`])
    pub fn account_dir(&self) -> PathBuf {
        self.root.join("account")
    }
    /// The device leases (`crate::lease`), shared
    pub fn devices_dir(&self) -> PathBuf {
        self.root.join("devices")
    }
    /// The background-WDA records (`crate::adopt`), shared and by device
    pub fn adopt_dir(&self) -> PathBuf {
        self.root.join("wda-adopt")
    }
    /// An update is being prepared (docs/dev/multi-instance-plan.md 5.3): no task is opened while it is there
    pub fn update_intent(&self) -> PathBuf {
        self.root.join("update-intent")
    }
    /// The lock whose holder runs the daily Apple ID session check for every instance
    pub fn account_check_lock(&self) -> PathBuf {
        self.root.join("account-check.lock")
    }
    /// The Apple ID account as this installation uses it ([`ictl_sign::account`]). With other instances an anisette
    /// state of unknown origin isn't used (docs/dev/multi-instance-plan.md 4.2)
    pub fn account(&self) -> ictl_sign::AccountDir {
        ictl_sign::AccountDir {
            dir: self.account_dir(),
            anisette_url: self.anisette_url.clone(),
            require_known_origin: self.shared_with_others(),
        }
    }
    /// Whether other instances share the data root: this is a named one, or named ones are registered or reserved. A
    /// registry that can't be read counts as shared
    pub fn shared_with_others(&self) -> bool {
        self.instance.is_some() || crate::instance::scan(&self.registry).map(|s| s.any()).unwrap_or(true)
    }
    /// Before a daemon starts: the registry and data root are the ones every instance uses
    /// ([`crate::instance::check_anchor`]); a named instance has its device and id, and its data directory where the
    /// data root is worked out from
    pub fn check_instance(&self) -> anyhow::Result<()> {
        let anchor = crate::instance::check_anchor(&self.root).map_err(anyhow::Error::msg)?;
        let Some(name) = &self.instance else { return Ok(()) };
        if self.udid.is_none() || self.instance_id.is_empty() {
            anyhow::bail!(
                "{}",
                ictl_l10n::trf!(
                    "实例 {} 缺 IPHONE_UDID 或 IPHONE_CONTROL_INSTANCE_ID：命名实例要按它的登记启动（--instance {}）",
                    "The instance {} has no IPHONE_UDID or IPHONE_CONTROL_INSTANCE_ID: a named instance starts from its registration (--instance {})",
                    name,
                    name
                )
            );
        }
        let at_home = self.data_dir.file_name().is_some_and(|f| f == name.as_str())
            && self.data_dir.parent().and_then(Path::file_name).is_some_and(|f| f == "instances");
        if anchor.is_none() && !at_home {
            anyhow::bail!(
                "{}",
                ictl_l10n::trf!(
                    "实例 {} 的数据目录 {} 不在 <数据根>/instances/{} 下，找不到共用的数据根",
                    "The instance {}'s data directory {} isn't <data root>/instances/{}: the shared data root can't be found",
                    name,
                    self.data_dir.display(),
                    name
                )
            );
        }
        Ok(())
    }
    pub fn wda_dir(&self) -> PathBuf {
        self.data_dir.join("wda")
    }
    pub fn wda_signed(&self) -> PathBuf {
        self.wda_dir().join("WDA-signed.ipa")
    }
    /// 未签名的 WDA 用的是数据目录里下载来的那份（不是镜像自带或 ICTL_WDA_UNSIGNED 指定的）
    pub fn wda_unsigned_is_cache(&self) -> bool {
        self.wda_unsigned == wda_unsigned_cache(&self.data_dir)
    }
    /// 守护进程自己下载的开发者镜像缓存（没设 `ICTL_DDI_DIR` 时用）
    pub fn ddi_cache(&self) -> PathBuf {
        self.data_dir.join("ddi")
    }
    /// The cache of the image a phone takes: `ddi/personalized/` for the classic personalized image (iOS 17–18),
    /// `ddi/` for the cryptex one
    pub fn ddi_cache_for(&self, legacy: bool) -> PathBuf {
        if legacy { self.ddi_cache().join("personalized") } else { self.ddi_cache() }
    }
    /// 旧版本的锁屏密码：只有一份，不知道是哪台设备的。USB 上只接着一台时归到那台名下（见
    /// `crate::core::Daemon::adopt_legacy_passcode`），接着多台时不用，免得把一台的密码输进另一台
    pub fn legacy_passcode_file(&self) -> PathBuf {
        self.root.join("secrets/passcode")
    }
    pub fn passcodes_dir(&self) -> PathBuf {
        self.root.join("secrets/passcodes")
    }
    /// 建好 [`Self::passcodes_dir`]，只有自己能进；新建的目录在上一级目录里也落盘（[`ictl_unlock::durable`]）
    pub fn ensure_passcodes_dir(&self) -> std::io::Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.passcodes_dir();
        ictl_unlock::durable::create_dir_all_from(&self.root, &dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }
    /// 这台设备的锁屏密码。UDID 来自 usbmuxd，只有字母、数字和 `-`；别的字符换掉，不让它变成路径
    pub fn passcode_file_for(&self, udid: &str) -> PathBuf {
        self.passcodes_dir().join(passcode_name(udid))
    }
    /// Saves `udid`'s passcode in place of the one it had: a crash leaves the old one or the new one whole, never a
    /// truncated file (which would read as no passcode, or as a shorter one)
    pub fn write_passcode(&self, udid: &str, code: &str) -> std::io::Result<()> {
        let _lock = self.passcodes_lock()?;
        let dir = self.ensure_passcodes_dir()?;
        let name = passcode_name(udid);
        ictl_unlock::durable::remove_temps(&dir, &name)?;
        ictl_unlock::durable::write_atomic(&dir, &name, code.as_bytes(), 0o600)
    }
    /// `passcode clear --device`: `udid`'s passcode gone, with any copy of it a crash left while it was being written
    /// The old passcode set aside for it by an adoption cut short goes first: removed after the passcode, a crash in
    /// between would leave it to be linked back as `udid`'s passcode
    pub fn clear_passcode(&self, udid: &str) -> std::io::Result<()> {
        let _lock = self.passcodes_lock()?;
        let dir = self.passcodes_dir();
        let name = passcode_name(udid);
        ictl_unlock::durable::remove(&dir, &format!("{LEGACY_STAGED}{name}"))?;
        ictl_unlock::durable::remove_temps(&dir, &name)?;
        ictl_unlock::durable::remove(&dir, &name)?;
        // the device that held up an older version's flag may have been this one. Not retiring it is the safe way
        // to fail: the passcode is cleared all the same
        let _ = self.retire_legacy_locked();
        Ok(())
    }
    /// An older version's flag on auto-unlock ([`ictl_unlock::DisableStore::retire_legacy`]) removed once every
    /// device with a saved passcode was released from it, unless a passcode whose device isn't known is still there
    /// (the old single one, or one set aside by an adoption cut short). Under the passcodes lock, so no passcode is
    /// saved meanwhile. `Ok(true)`: removed
    pub fn retire_legacy_unlock_disabled(&self) -> std::io::Result<bool> {
        let _lock = self.passcodes_lock()?;
        self.retire_legacy_locked()
    }
    fn retire_legacy_locked(&self) -> std::io::Result<bool> {
        if self.legacy_passcode_file().try_exists()? {
            return Ok(false);
        }
        let mut devices = Vec::new();
        match std::fs::read_dir(self.passcodes_dir()) {
            Ok(entries) => {
                for e in entries {
                    let name = e?.file_name().to_string_lossy().into_owned();
                    if name.starts_with(LEGACY_STAGED) {
                        return Ok(false);
                    }
                    if !name.starts_with('.') {
                        devices.push(name);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.unlock_disabled().retire_legacy(&devices)
    }
    /// `passcode clear --all`: every device's passcode and the one an older version saved, under the same lock as
    /// [`Self::adopt_legacy_passcode`], so an adoption under way can't publish the old one again afterwards
    pub fn clear_all_passcodes(&self) -> std::io::Result<()> {
        let _lock = self.passcodes_lock()?;
        match std::fs::remove_dir_all(self.passcodes_dir()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let legacy = self.legacy_passcode_file();
        let secrets = legacy.parent().unwrap_or(Path::new("."));
        ictl_unlock::durable::remove(secrets, "passcode")?;
        if secrets.is_dir() {
            ictl_unlock::durable::sync_dir(secrets)?;
        }
        // no passcode left: no device is unlocked automatically, so an older version's flag has nothing to guard
        let _ = self.retire_legacy_locked();
        Ok(())
    }
    /// Whether the passcode an older version saved was set aside for `udid` by an adoption cut short: only `udid`
    /// may finish it
    pub fn legacy_passcode_staged_for(&self, udid: &str) -> bool {
        self.passcodes_dir().join(format!("{LEGACY_STAGED}{}", passcode_name(udid))).exists()
    }
    /// The single passcode an older version saved, given to `udid` (the only device connected) unless it has one of
    /// its own. Two steps, each on disk before the next, under a lock shared with [`Self::clear_all_passcodes`]:
    /// 1. the old file is moved aside under a name bound to `udid` (it is no one's to take from then on);
    /// 2. that is linked as `udid`'s passcode (a link fails if the name is taken: one set meanwhile is never
    ///    overwritten), then the name set aside is removed.
    ///
    /// Cut short after step 1, only `udid` finishes it; a passcode set aside for another device is left alone.
    /// `Ok(false)`: nothing given (no old passcode, `udid` has its own, or it was set aside for another device)
    pub fn adopt_legacy_passcode(&self, udid: &str) -> std::io::Result<bool> {
        let _lock = self.passcodes_lock()?;
        let dir = self.ensure_passcodes_dir()?;
        let mine = format!("{LEGACY_STAGED}{}", passcode_name(udid));
        let mut set_aside = false;
        for e in std::fs::read_dir(&dir)? {
            let name = e?.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(LEGACY_STAGED) {
                if name != mine {
                    // bound to another device by an adoption cut short: never this one's
                    return Ok(false);
                }
                set_aside = true;
            }
        }
        let target = self.passcode_file_for(udid);
        if !set_aside {
            if target.exists() {
                return Ok(false);
            }
            let legacy = self.legacy_passcode_file();
            match std::fs::rename(&legacy, dir.join(&mine)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e),
            }
            ictl_unlock::durable::sync_dir(&dir)?;
            ictl_unlock::durable::sync_dir(legacy.parent().unwrap_or(Path::new(".")))?;
        }
        // an older version may have left it readable by others: private before it becomes this device's passcode
        {
            use std::os::unix::fs::PermissionsExt;
            let f = std::fs::File::open(dir.join(&mine))?;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            f.sync_all()?;
        }
        let given = match std::fs::hard_link(dir.join(&mine), &target) {
            Ok(()) => true,
            // set for this device meanwhile: that one stays, and the old one, bound to it, goes
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e),
        };
        ictl_unlock::durable::sync_dir(&dir)?;
        ictl_unlock::durable::remove(&dir, &mine)?;
        Ok(given)
    }
    fn passcodes_lock(&self) -> std::io::Result<std::fs::File> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let secrets = self.root.join("secrets");
        ictl_unlock::durable::create_dir_all_from(&self.root, &secrets)?;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(secrets.join("passcodes.lock"))?;
        // SAFETY: a valid descriptor, owned by `f` for as long as the lock is used
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(f)
    }
    /// The single flag 0.3.2 and earlier kept for all devices; still written, as the shadow of
    /// [`Self::unlock_disabled`], so a downgraded daemon keeps every device disabled
    /// In the data root's `logs/` (the default instance's, where 0.3.2 looks), shared by every instance
    pub fn unlock_disabled_flag(&self) -> PathBuf {
        self.root.join("logs/UNLOCK-DISABLED")
    }
    /// Auto-unlock disabled after a failure, per device ([`ictl_unlock::DisableStore`]), shared by every instance
    pub fn unlock_disabled(&self) -> ictl_unlock::DisableStore {
        ictl_unlock::DisableStore::new(self.root.join("secrets/unlock-disabled"), self.unlock_disabled_flag())
    }
    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }
    /// A marker per phone with a HID session open (`crate::hidmark`): by device, so shared by every instance (the
    /// next instance to take the device clears what another left)
    pub fn hid_marker_dir(&self) -> PathBuf {
        self.root.join("hid-open")
    }
    /// Persistent daemon lifecycle marker and anti-storm alert state (`crate::watchdog`)
    pub fn run_state_file(&self) -> PathBuf {
        self.data_dir.join("run-state.json")
    }

    /// 建好数据目录并收紧权限：账号私钥、锁屏密码都在里面（配对记录归系统的 usbmuxd 管）
    pub fn prepare_dirs(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        // first, before any plain create_dir_all: the data directory and every directory above it made now, then
        // secrets/ and logs/, each kept in its parent. Auto-unlock writes durably inside those (passcodes, disabled
        // records, their shadow); a directory above them made by a plain call could be lost with a power cut,
        // taking them along ([`ictl_unlock::durable`])
        if self.root != self.data_dir {
            ictl_unlock::durable::create_tree(&self.root, &["secrets", "logs"])?;
        }
        ictl_unlock::durable::create_tree(&self.data_dir, &["secrets", "logs"])?;
        for d in ["wda", "ddi"] {
            std::fs::create_dir_all(self.data_dir.join(d))?;
        }
        std::fs::create_dir_all(self.account_dir())?;
        let private =
            [self.data_dir.clone(), self.data_dir.join("secrets"), self.root.clone(), self.root.join("secrets")];
        for d in private.iter().chain([&self.account_dir()]) {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first start where the data directory and the ones above it don't exist yet: everything is made, and a
    /// passcode can be written (the order of the syncs is tested in `ictl_unlock::durable`)
    #[test]
    fn a_first_start_makes_the_data_directory_and_above() {
        let root = std::env::temp_dir().join(format!("ictl-config-cold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("new/share/iphone-control");
        let cfg = Config::from_env(&data);
        cfg.prepare_dirs().unwrap();
        for d in ["secrets", "logs", "account", "wda", "ddi"] {
            assert!(data.join(d).is_dir(), "{d}");
        }
        cfg.write_passcode("A", "123456").unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A passcode set again replaces the old one whole and stays private; the single passcode an older version
    /// saved is given to a device only if it has none, and never written over one set meanwhile
    #[test]
    fn passcodes_are_written_whole_and_an_old_one_never_replaces_a_new_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ictl-config-passcode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config::from_env(&dir);
        cfg.write_passcode("A", "123456").unwrap();
        cfg.write_passcode("A", "2468").unwrap();
        let f = cfg.passcode_file_for("A");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "2468");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);

        std::fs::write(cfg.legacy_passcode_file(), "1111").unwrap();
        assert!(!cfg.adopt_legacy_passcode("A").unwrap(), "A has its own");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "2468");
        assert!(cfg.legacy_passcode_file().is_file(), "kept: nobody got it");
        assert!(cfg.adopt_legacy_passcode("B").unwrap());
        assert_eq!(std::fs::read_to_string(cfg.passcode_file_for("B")).unwrap(), "1111");
        assert!(!cfg.legacy_passcode_file().exists());
        let left: Vec<_> = std::fs::read_dir(cfg.passcodes_dir()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 2, "no temporary file left: {left:?}");

        // a copy a crash left while writing A's passcode goes with `passcode clear --device`
        let stray = cfg.passcodes_dir().join(".A.tmp-1-0");
        std::fs::write(&stray, "9999").unwrap();
        cfg.clear_passcode("A").unwrap();
        assert!(!stray.exists() && !cfg.passcode_file_for("A").exists());
        assert!(cfg.passcode_file_for("B").exists(), "B's stays");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An older version's flag goes once every device with a saved passcode is released from it, not while a
    /// passcode whose device isn't known is there; clearing the passcode of the one left also retires it
    #[test]
    fn an_older_versions_flag_goes_once_every_device_with_a_passcode_is_released() {
        let dir = std::env::temp_dir().join(format!("ictl-config-retire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config::from_env(&dir);
        cfg.prepare_dirs().unwrap();
        cfg.write_passcode("A", "1111").unwrap();
        cfg.write_passcode("B", "2222").unwrap();
        std::fs::write(cfg.unlock_disabled_flag(), "1700000000 x\n").unwrap();
        let store = cfg.unlock_disabled();
        store.migrate().unwrap();
        store.reset("A").unwrap();
        assert!(!cfg.retire_legacy_unlock_disabled().unwrap(), "B isn't released");
        store.reset("B").unwrap();
        std::fs::write(cfg.legacy_passcode_file(), "3333").unwrap();
        assert!(!cfg.retire_legacy_unlock_disabled().unwrap(), "an old passcode of an unknown device");
        std::fs::remove_file(cfg.legacy_passcode_file()).unwrap();
        std::fs::write(cfg.passcodes_dir().join(format!("{LEGACY_STAGED}C")), "3333").unwrap();
        assert!(!cfg.retire_legacy_unlock_disabled().unwrap(), "one set aside for C, not adopted yet");
        std::fs::remove_file(cfg.passcodes_dir().join(format!("{LEGACY_STAGED}C"))).unwrap();
        assert!(cfg.retire_legacy_unlock_disabled().unwrap());
        assert!(!store.legacy() && !cfg.unlock_disabled_flag().exists());

        // B left unreleased, then its passcode cleared: nothing holds the flag up any more
        std::fs::write(cfg.unlock_disabled_flag(), "1700000100 x\n").unwrap();
        store.migrate().unwrap();
        store.reset("A").unwrap();
        cfg.clear_passcode("B").unwrap();
        assert!(!store.legacy() && !cfg.unlock_disabled_flag().exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An adoption cut short binds the old passcode to the device it was for: whether it stopped before or after the
    /// link, another device connected alone later never gets it, and the first one finishes
    #[test]
    fn an_old_passcode_cut_short_goes_to_its_device_only() {
        let dir = std::env::temp_dir().join(format!("ictl-config-adopt-cut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config::from_env(&dir);
        let staged = |u: &str| cfg.passcodes_dir().join(format!("{LEGACY_STAGED}{u}"));
        // cut after the link, before the name set aside was removed
        cfg.ensure_passcodes_dir().unwrap();
        std::fs::write(staged("A"), "1111").unwrap();
        std::fs::hard_link(staged("A"), cfg.passcode_file_for("A")).unwrap();
        assert!(cfg.legacy_passcode_staged_for("A") && !cfg.legacy_passcode_staged_for("B"));
        assert!(!cfg.adopt_legacy_passcode("B").unwrap());
        assert!(!cfg.passcode_file_for("B").exists(), "never B's");
        assert!(!cfg.adopt_legacy_passcode("A").unwrap(), "A has it already");
        assert!(!staged("A").exists(), "finished");
        assert!(!cfg.adopt_legacy_passcode("B").unwrap(), "and gone for good");
        std::fs::remove_dir_all(&dir).unwrap();

        // cut after moving it aside, before the link
        let cfg = Config::from_env(&dir);
        cfg.ensure_passcodes_dir().unwrap();
        std::fs::write(staged("A"), "1111").unwrap();
        assert!(!cfg.adopt_legacy_passcode("B").unwrap());
        assert!(!cfg.passcode_file_for("B").exists());
        assert!(cfg.adopt_legacy_passcode("A").unwrap());
        assert_eq!(std::fs::read_to_string(cfg.passcode_file_for("A")).unwrap(), "1111");
        std::fs::remove_dir_all(&dir).unwrap();

        // cut short, then `passcode clear --device A`: the next round doesn't link it back
        let cfg = Config::from_env(&dir);
        cfg.ensure_passcodes_dir().unwrap();
        std::fs::write(staged("A"), "1111").unwrap();
        std::fs::hard_link(staged("A"), cfg.passcode_file_for("A")).unwrap();
        cfg.clear_passcode("A").unwrap();
        assert!(!staged("A").exists() && !cfg.passcode_file_for("A").exists());
        assert!(!cfg.adopt_legacy_passcode("A").unwrap() && !cfg.passcode_file_for("A").exists());
        std::fs::remove_dir_all(&dir).unwrap();

        // an old passcode left readable by others becomes the device's private one
        let cfg = Config::from_env(&dir);
        cfg.ensure_passcodes_dir().unwrap();
        std::fs::write(cfg.legacy_passcode_file(), "1111").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(cfg.legacy_passcode_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(cfg.adopt_legacy_passcode("A").unwrap());
            let mode = std::fs::metadata(cfg.passcode_file_for("A")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();

        // cleared meanwhile: nothing comes back
        let cfg = Config::from_env(&dir);
        cfg.ensure_passcodes_dir().unwrap();
        std::fs::write(cfg.legacy_passcode_file(), "1111").unwrap();
        std::fs::write(staged("A"), "1111").unwrap();
        cfg.clear_all_passcodes().unwrap();
        assert!(!cfg.adopt_legacy_passcode("A").unwrap() && !cfg.adopt_legacy_passcode("B").unwrap());
        assert!(!cfg.passcode_file_for("A").exists() && !cfg.passcode_file_for("B").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A debug print of the settings shows whether a key is set, never the key
    #[test]
    fn notification_keys_stay_out_of_debug_output() {
        let n = NotifyConfig {
            tg_bot_token: Some("123:secret-token".into()),
            tg_chat_id: Some("42".into()),
            bark_server: "https://api.day.app".into(),
            bark_key: Some("bark-secret".into()),
        };
        let s = format!("{n:?}");
        assert!(!s.contains("secret") && s.contains("<set>") && s.contains("api.day.app"), "{s}");
    }
}
