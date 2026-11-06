//! 守护进程配置：全部来自环境变量（容器里由 .env 注入；从源码运行时由 systemd 的环境文件或 shell 给），都有默认值。秘密只放文件，不放环境变量以外的地方。

use std::path::{Path, PathBuf};
use std::time::Duration;

use ictl_device::ScreenSize;

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub socket: PathBuf,
    /// 固定要控制的手机（`IPHONE_UDID`）；None 时可以在运行中选（见 [`crate::core::Persisted::selected_udid`]），
    /// 都没选就用第一台
    pub udid: Option<String>,
    /// 由守护进程拉起并看护 usbmuxd。默认以 root 运行（容器）时为 true；普通用户拉起的 usbmuxd 碰不到 USB，
    /// 改用系统的 usbmuxd 服务。宿主机以 root 运行、又已有 usbmuxd 服务时设 ICTL_MANAGE_USBMUXD=0
    pub manage_usbmuxd: bool,
    pub usbmuxd_bin: String,
    /// 未签名的 WDA：镜像里自带；从源码运行时是数据目录里的 [`Config::wda_unsigned_cache`]，第一次签名前下载
    pub wda_unsigned: PathBuf,
    pub ddi_url: String,
    /// 自己从 Xcode 提取的 DDI Restore 目录；设了就不下载
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

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
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
            manage_usbmuxd: env("ICTL_MANAGE_USBMUXD")
                .map_or_else(crate::protocol::running_as_root, |v| v != "0" && v != "false"),
            usbmuxd_bin: env("ICTL_USBMUXD_BIN").unwrap_or_else(|| "usbmuxd".into()),
            wda_unsigned: env("ICTL_WDA_UNSIGNED").map(PathBuf::from).unwrap_or_else(|| {
                let image = Path::new(crate::wda_pkg::IMAGE_PATH);
                if image.is_file() { image.to_owned() } else { wda_unsigned_cache(data_dir) }
            }),
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
    pub fn ddi_cache(&self) -> PathBuf {
        self.ddi_dir.clone().unwrap_or_else(|| self.data_dir.join("ddi"))
    }
    /// 旧版本的锁屏密码：只有一份，不知道是哪台设备的。USB 上只接着一台时归到那台名下（见
    /// [`crate::core::Daemon::adopt_legacy_passcode`]），接着多台时不用，免得把一台的密码输进另一台
    pub fn legacy_passcode_file(&self) -> PathBuf {
        self.data_dir.join("secrets/passcode")
    }
    pub fn passcodes_dir(&self) -> PathBuf {
        self.data_dir.join("secrets/passcodes")
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

    /// 建好数据目录并收紧权限：配对记录、账号私钥、锁屏密码都在里面
    pub fn prepare_dirs(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for d in ["lockdown", "account", "wda", "ddi", "secrets", "logs"] {
            std::fs::create_dir_all(self.data_dir.join(d))?;
        }
        for d in ["", "lockdown", "account", "secrets"] {
            std::fs::set_permissions(self.data_dir.join(d), std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}
