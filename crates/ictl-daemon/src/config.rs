//! 守护进程配置：全部来自环境变量（服务从 ~/.config/iphone-control/env 读入，手动运行时由 shell 给），都有默认值。秘密只放文件，不放环境变量以外的地方。

use std::path::{Path, PathBuf};
use std::time::Duration;

use ictl_device::ScreenSize;

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
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

/// A new file in `dir` holding `text`, private (0600 from the start) and synced; its name is unique to this call
fn secret_tmp(dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".tmp-{}-{n}", std::process::id()));
    let r = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).and_then(|mut f| {
        f.write_all(text.as_bytes())?;
        f.sync_all()
    });
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(tmp)
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
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
        Self {
            data_dir: data_dir.to_owned(),
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
    pub fn account_dir(&self) -> PathBuf {
        self.data_dir.join("account")
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
        self.data_dir.join("secrets/passcode")
    }
    pub fn passcodes_dir(&self) -> PathBuf {
        self.data_dir.join("secrets/passcodes")
    }
    /// 建好 [`Self::passcodes_dir`]，只有自己能进
    pub fn ensure_passcodes_dir(&self) -> std::io::Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.passcodes_dir();
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }
    /// 这台设备的锁屏密码。UDID 来自 usbmuxd，只有字母、数字和 `-`；别的字符换掉，不让它变成路径
    pub fn passcode_file_for(&self, udid: &str) -> PathBuf {
        let name: String = udid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        self.passcodes_dir().join(name)
    }
    /// Saves `udid`'s passcode in place of the one it had: a crash leaves the old one or the new one whole, never a
    /// truncated file (which would read as no passcode, or as a shorter one)
    pub fn write_passcode(&self, udid: &str, code: &str) -> std::io::Result<()> {
        let dir = self.ensure_passcodes_dir()?;
        let tmp = secret_tmp(&dir, code)?;
        let r = std::fs::rename(&tmp, self.passcode_file_for(udid)).and_then(|_| sync_dir(&dir));
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r
    }
    /// The single passcode an older version saved, given to `udid` unless it has one of its own already (one set
    /// with `passcode set` a moment ago is never overwritten). Published whole: a hard link to a synced copy, which
    /// fails if the name is taken. `Ok(false)`: `udid` had a passcode, the old one is left alone
    pub fn adopt_legacy_passcode(&self, udid: &str) -> std::io::Result<bool> {
        let legacy = self.legacy_passcode_file();
        let code = std::fs::read_to_string(&legacy)?;
        let dir = self.ensure_passcodes_dir()?;
        let tmp = secret_tmp(&dir, &code)?;
        let r = std::fs::hard_link(&tmp, self.passcode_file_for(udid));
        let _ = std::fs::remove_file(&tmp);
        match r {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(e),
        }
        sync_dir(&dir)?;
        std::fs::remove_file(&legacy)?;
        sync_dir(legacy.parent().unwrap_or(Path::new(".")))?;
        Ok(true)
    }
    /// The single flag 0.3.2 and earlier kept for all devices; still written, as the shadow of
    /// [`Self::unlock_disabled`], so a downgraded daemon keeps every device disabled
    pub fn unlock_disabled_flag(&self) -> PathBuf {
        self.logs().join("UNLOCK-DISABLED")
    }
    /// Auto-unlock disabled after a failure, per device ([`ictl_unlock::DisableStore`])
    pub fn unlock_disabled(&self) -> ictl_unlock::DisableStore {
        ictl_unlock::DisableStore::new(self.data_dir.join("secrets/unlock-disabled"), self.unlock_disabled_flag())
    }
    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }
    /// A marker per phone with a HID session open (`crate::hidmark`)
    pub fn hid_marker_dir(&self) -> PathBuf {
        self.data_dir.join("hid-open")
    }
    /// Persistent daemon lifecycle marker and anti-storm alert state (`crate::watchdog`)
    pub fn run_state_file(&self) -> PathBuf {
        self.data_dir.join("run-state.json")
    }

    /// 建好数据目录并收紧权限：账号私钥、锁屏密码都在里面（配对记录归系统的 usbmuxd 管）
    pub fn prepare_dirs(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for d in ["account", "wda", "ddi", "secrets", "logs"] {
            std::fs::create_dir_all(self.data_dir.join(d))?;
        }
        for d in ["", "account", "secrets"] {
            std::fs::set_permissions(self.data_dir.join(d), std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
