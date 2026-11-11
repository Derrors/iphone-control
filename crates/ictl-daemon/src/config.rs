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
    /// 未签名的 WDA：默认是数据目录里的 [`Config::wda_unsigned_cache`]，第一次签名前下载
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
    /// 实验（`ICTL_WDA_DETACHED=1`）：在后台启动 WDA、不持有 XCTest 会话（见 `docs/dev/wda-dependency-research.md`）
    pub wda_detached: bool,
}

#[derive(Debug, Clone, Default)]
pub struct NotifyConfig {
    pub tg_bot_token: Option<String>,
    pub tg_chat_id: Option<String>,
    pub bark_server: String,
    pub bark_key: Option<String>,
}

impl NotifyConfig {
    pub fn any(&self) -> bool {
        (self.tg_bot_token.is_some() && self.tg_chat_id.is_some()) || self.bark_key.is_some()
    }
}

/// 读环境变量，去掉首尾空白，空值当作没设（配置文件里常留着 `KEY=`）
pub fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Where to look for ffmpeg (the viewer's H.264 picture; on macOS only the fallback for the native VideoToolbox
/// encoder), in order: `ICTL_FFMPEG`, else `ffmpeg` on `PATH` and, on macOS, Homebrew's places (a launchd service
/// doesn't have them on its `PATH`)
pub fn ffmpeg_paths() -> Vec<PathBuf> {
    if let Some(p) = env("ICTL_FFMPEG") {
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

fn env_num<T: std::str::FromStr>(k: &str, default: T) -> T {
    env(k).and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Config {
    pub fn from_env(data_dir: &Path) -> Self {
        let screen_fallback =
            match (env("SCREEN_W").and_then(|v| v.parse().ok()), env("SCREEN_H").and_then(|v| v.parse().ok())) {
                (Some(width), Some(height)) => Some(ScreenSize { width, height }),
                _ => None,
            };
        Self {
            data_dir: data_dir.to_owned(),
            // 实际值由命令行给（--socket / ICTL_SOCKET，见 apps/iphone-control）
            socket: crate::protocol::default_socket(),
            udid: env("IPHONE_UDID"),
            wda_unsigned: env("ICTL_WDA_UNSIGNED").map(PathBuf::from).unwrap_or_else(|| wda_unsigned_cache(data_dir)),
            ddi_url: env("ICTL_DDI_URL").unwrap_or_else(|| ictl_device::ddi::DEFAULT_BASE_URL.into()),
            ddi_dir: env("ICTL_DDI_DIR").map(PathBuf::from),
            anisette_url: env("ICTL_ANISETTE_URL"),
            screen_fallback,
            resign_min: Duration::from_secs(env_num("RESIGN_MIN_DAYS", 2u64) * 86400),
            resign_check: Duration::from_secs(env_num("RESIGN_CHECK_HOURS", 6u64) * 3600),
            session_check: Duration::from_secs(24 * 3600),
            offline_alert: Duration::from_secs(env_num("OFFLINE_ALERT_MIN", 10u64) * 60),
            notify: NotifyConfig {
                tg_bot_token: env("TG_BOT_TOKEN"),
                tg_chat_id: env("TG_CHAT_ID"),
                bark_server: env("BARK_SERVER").unwrap_or_else(|| "https://api.day.app".into()),
                bark_key: env("BARK_KEY"),
            },
            wda_detached: env("ICTL_WDA_DETACHED").is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes")),
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
    /// 旧版本的锁屏密码：只有一份，不知道是哪台设备的。USB 上只接着一台时归到那台名下（见
    /// [`crate::core::Daemon::adopt_legacy_passcode`]），接着多台时不用，免得把一台的密码输进另一台
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
    pub fn unlock_disabled_flag(&self) -> PathBuf {
        self.logs().join("UNLOCK-DISABLED")
    }
    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
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
