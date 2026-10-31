//! 设备层：一台 iPhone 的全部底层能力，基于 `idevice`。
//!
//! - 发现与监听：usbmuxd `get_devices` / `listen`
//! - 隧道：CoreDeviceProxy + jktcp 用户态 TCP 栈（免 root），RSD 握手
//! - 开发者镜像：`cryptexd::installed_ddi` / `install_ddi`（个性化 + TSS）；DDI 下载由本 crate 负责
//! - HID：universalhidservice（触摸、键盘）、indigo（硬件按键）；**必须同时持有显示媒体流**，否则事件被静默丢弃
//! - 截图：CoreDevice screencaptureservice
//! - App：installation_proxy（列出时过滤 `SBAppTags` 含 `hidden` 的后台服务）、安装 IPA
//!
//! 不负责：WDA（见 `ictl-wda`）、状态机（见 `ictl-daemon`）。

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("设备未连接")]
    NotConnected,
    #[error("设备未配对，需要在手机上点「信任」")]
    NotPaired,
    #[error("开发者模式未开启")]
    DeveloperModeOff,
    /// 锁屏且自开机以来从未解锁（BFU），隧道建不起来，只能人工解锁
    #[error("手机锁屏（PasswordProtected），需要人工解锁")]
    PasswordProtected,
    #[error("开发者镜像未挂载")]
    DdiNotMounted,
    #[error("idevice: {0}")]
    Idevice(String),
}

pub type Result<T> = std::result::Result<T, DeviceError>;

/// usbmuxd 报告的一台设备
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub udid: String,
    pub product_type: String,
    pub product_version: String,
}

/// 屏幕逻辑尺寸（点）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenSize {
    pub width: f64,
    pub height: f64,
}

/// 已安装 App（已过滤 hidden 后台服务）
#[derive(Debug, Clone)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: String,
    pub system: bool,
}

/// usbmuxd 监听事件
#[derive(Debug, Clone)]
pub enum UsbEvent {
    Attached(DeviceInfo),
    Detached { udid: String },
}

/// 检查 sysfs 里是否有 Apple（05ac）USB 设备。容器无 udev 时用来判断"usbmuxd 是否漏掉了设备"。
pub fn apple_usb_present() -> bool {
    todo!("扫描 /sys/bus/usb/devices/*/idVendor")
}

pub async fn list_devices() -> Result<Vec<DeviceInfo>> {
    todo!("UsbmuxdConnection::default().get_devices()")
}

pub async fn watch_devices() -> Result<tokio::sync::mpsc::Receiver<UsbEvent>> {
    todo!("UsbmuxdConnection::listen()")
}

/// 一台设备的句柄。隧道、HID 会话都挂在它上面，由守护进程独占持有。
pub struct Device {
    pub info: DeviceInfo,
}

impl Device {
    pub async fn open(udid: &str) -> Result<Self> {
        let _ = udid;
        todo!("取 usbmuxd provider、读配对记录")
    }

    pub async fn developer_mode_enabled(&self) -> Result<bool> {
        todo!("amfi")
    }

    pub async fn reveal_developer_mode(&self) -> Result<()> {
        todo!("amfi reveal")
    }

    /// 建立 CoreDeviceProxy 用户态隧道并完成 RSD 握手
    pub async fn open_tunnel(&mut self) -> Result<Tunnel> {
        todo!("CoreDeviceProxy::create_software_tunnel + RsdHandshake")
    }
}

/// 一条活着的隧道。HID、截图、cryptexd、XCTest 都经它访问 RSD 服务。
pub struct Tunnel {
    _private: (),
}

impl Tunnel {
    pub async fn ddi_mounted(&mut self) -> Result<bool> {
        todo!("cryptexd::installed_ddi")
    }

    /// 个性化并安装开发者镜像。`restore_dir` 是解包好的 DDI Restore 目录（见 [`ensure_ddi_cache`]）
    pub async fn mount_ddi(&mut self, restore_dir: &std::path::Path) -> Result<()> {
        let _ = restore_dir;
        todo!("Cryptex1Assets::load + cryptexd::install_ddi")
    }

    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        todo!("screencaptureservices::take_screenshot(ImageFormat::Png)")
    }

    /// 开启 HID 会话：同时启动显示媒体流（RTP 丢弃）
    pub async fn hid(&mut self) -> Result<HidSession> {
        todo!("display_stream + UniversalHidServiceClient + IndigoHidClient")
    }

    pub async fn list_apps(&mut self, include_system: bool) -> Result<Vec<AppInfo>> {
        let _ = include_system;
        todo!("installation_proxy::get_apps，过滤 SBAppTags 含 hidden")
    }

    pub async fn install_ipa(&mut self, ipa: &std::path::Path) -> Result<()> {
        let _ = ipa;
        todo!("AFC 上传 + installation_proxy::install")
    }
}

/// HID 硬件按键
#[derive(Debug, Clone, Copy)]
pub enum Button {
    Home,
    Lock,
    VolumeUp,
    VolumeDown,
}

/// HID 会话。坐标统一用点，内部换算成 0..=65535。
pub struct HidSession {
    pub screen: ScreenSize,
}

impl HidSession {
    pub async fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        let _ = (x, y);
        todo!("UniversalHidServiceClient::tap")
    }

    pub async fn drag(&mut self, from: (f64, f64), to: (f64, f64), seconds: f64) -> Result<()> {
        let _ = (from, to, seconds);
        todo!("UniversalHidServiceClient::drag")
    }

    pub async fn press(&mut self, button: Button) -> Result<()> {
        let _ = button;
        todo!("IndigoHidClient::send_button")
    }

    /// 只能输入键盘能直接按出的 ASCII
    pub async fn type_ascii(&mut self, text: &str) -> Result<()> {
        let _ = text;
        todo!("MainKeyboardService")
    }
}

/// 点 → HID 归一化坐标
pub fn to_hid(x: f64, y: f64, screen: ScreenSize) -> (u16, u16) {
    let n = |v: f64, max: f64| (v / max * 65535.0).round().clamp(0.0, 65535.0) as u16;
    (n(x, screen.width), n(y, screen.height))
}

/// 确保本地有与设备匹配的 DDI 缓存，返回 Restore 目录。苹果文件，不能随项目分发。
pub async fn ensure_ddi_cache(cache_dir: &std::path::Path) -> Result<PathBuf> {
    let _ = cache_dir;
    todo!("按配置的来源下载并解包")
}
