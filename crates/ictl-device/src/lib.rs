//! 设备层：一台 iPhone 的全部底层能力，基于 `idevice`。
//!
//! - 发现：usbmuxd `get_devices`；容器里没有 udev，另用 sysfs 判断 USB 上是否有 Apple 设备（[`apple_usb_present`]）
//! - 配对、设备信息、开发者模式：lockdown / amfi
//! - 隧道：CoreDeviceProxy + jktcp 用户态 TCP 栈（免 root、不建网卡），RSD 握手（[`Tunnel`]）
//! - 开发者镜像：`cryptexd::installed_ddi` / `install_ddi`（个性化 + TSS）；DDI 下载见 [`ddi`]
//! - HID：universalhidservice（触摸）、indigo（按键、键盘）；**必须同时持有显示媒体流**（[`hid::HidSession`]）
//! - 截图：CoreDevice screencaptureservice
//! - App：installation_proxy（列出时过滤 `SBAppTags` 含 `hidden` 的后台服务，见 [`apps`]）、安装 IPA
//!
//! 不负责：WDA（见 `ictl-wda`）、状态机（见 `ictl-daemon`）。

pub mod apps;
pub mod ddi;
pub mod hid;
mod media;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use idevice::{
    IdeviceError, IdeviceService, RsdService,
    amfi::AmfiClient,
    core_device::{ImageFormat, ScreenCaptureServiceClient},
    core_device_proxy::CoreDeviceProxy,
    cryptexd::{Cryptex1Assets, install_ddi, installed_ddi},
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    provider::{IdeviceProvider, UsbmuxdProvider},
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
    usbmuxd::{Connection, UsbmuxdAddr},
};

pub use apps::AppInfo;
pub use hid::{Button, HidSession};

/// 连接标签，出现在设备日志里
pub const LABEL: &str = "iphone-control";

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("设备未连接")]
    NotConnected,
    #[error("设备未配对，需要在手机上点「信任」")]
    NotPaired,
    #[error("手机上的「信任此电脑」弹窗还在等待确认")]
    PairingPending,
    #[error("用户在手机上拒绝了配对")]
    PairingDenied,
    #[error("开发者模式未开启")]
    DeveloperModeOff,
    /// 锁屏且自开机以来从未解锁（BFU），或锁屏时建隧道，都会报这个
    #[error("手机锁屏（PasswordProtected），需要解锁")]
    PasswordProtected,
    #[error("下载开发者镜像失败：{0}")]
    Download(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("idevice：{0}")]
    Idevice(String),
    /// 手机上的服务没在限定时间内应答。调用方应丢掉隧道重建
    #[error("超时：{0}")]
    Timeout(String),
    /// 调用方给的参数不合法（例如 HID 不能输入的字符）。与连接无关，不需要重建会话
    #[error("{0}")]
    InvalidInput(String),
}

/// 给对手机的一次调用加时限：卡住的服务不能把调用方（守护进程持锁时）一起卡死
pub(crate) async fn limit<T>(what: &str, d: Duration, f: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(d, f).await.map_err(|_| DeviceError::Timeout(format!("{what}（{} 秒）", d.as_secs())))?
}

impl From<IdeviceError> for DeviceError {
    fn from(e: IdeviceError) -> Self {
        match e {
            IdeviceError::PasswordProtected => Self::PasswordProtected,
            IdeviceError::DeviceNotFound => Self::NotConnected,
            IdeviceError::InvalidHostID => Self::NotPaired,
            IdeviceError::PairingDialogResponsePending => Self::PairingPending,
            IdeviceError::UserDeniedPairing => Self::PairingDenied,
            IdeviceError::DeveloperModeNotEnabled => Self::DeveloperModeOff,
            other => Self::Idevice(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, DeviceError>;

/// 屏幕逻辑尺寸（点）
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScreenSize {
    pub width: f64,
    pub height: f64,
}

/// usbmuxd 报告的一台 USB 设备
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub udid: String,
    pub device_id: u32,
}

/// 设备基本信息（lockdown 不开会话就能读）
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub product_type: String,
    pub product_version: String,
}

impl DeviceInfo {
    /// iOS 主版本号，读不出返回 0
    pub fn major_version(&self) -> u32 {
        self.product_version.split('.').next().and_then(|s| s.parse().ok()).unwrap_or(0)
    }
}

/// usbmuxd 地址：环境变量 `USBMUXD_SOCKET_ADDRESS` 优先，否则默认 `/var/run/usbmuxd`
pub fn usbmuxd_addr() -> UsbmuxdAddr {
    UsbmuxdAddr::from_env_var().unwrap_or_default()
}

/// 检查 sysfs 里是否有 Apple（05ac）USB 设备。容器无 udev 时用来判断 usbmuxd 是否漏掉了设备。
/// `root` 一般是 `/sys/bus/usb/devices`。
pub fn apple_usb_present(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else { return false };
    entries
        .flatten()
        .any(|e| std::fs::read_to_string(e.path().join("idVendor")).map(|v| v.trim() == "05ac").unwrap_or(false))
}

/// usbmuxd 看到的 USB 设备（不含网络设备）。usbmuxd 没在运行时返回错误。
pub async fn list_devices() -> Result<Vec<UsbDevice>> {
    // 不用 UsbmuxdConnection::default()：它固定连 /var/run/usbmuxd，不看 USBMUXD_SOCKET_ADDRESS
    let mut mux = usbmuxd_addr().connect(0).await?;
    Ok(mux
        .get_devices()
        .await?
        .into_iter()
        .filter(|d| matches!(d.connection_type, Connection::Usb))
        .map(|d| UsbDevice { udid: d.udid, device_id: d.device_id })
        .collect())
}

/// 一台设备的句柄。只持有 usbmuxd provider，每次调用按需新建连接；隧道见 [`Device::open_tunnel`]。
#[derive(Debug, Clone)]
pub struct Device {
    pub udid: String,
    provider: Arc<UsbmuxdProvider>,
}

impl Device {
    /// `udid` 为空时取 usbmuxd 看到的第一台 USB 设备
    pub async fn find(udid: Option<&str>) -> Result<Self> {
        let devices = list_devices().await?;
        let dev = match udid {
            Some(u) => devices.into_iter().find(|d| d.udid == u),
            None => devices.into_iter().next(),
        }
        .ok_or(DeviceError::NotConnected)?;
        let provider = UsbmuxdProvider {
            addr: usbmuxd_addr(),
            tag: dev.device_id,
            udid: dev.udid.clone(),
            device_id: dev.device_id,
            label: LABEL.into(),
        };
        Ok(Self { udid: dev.udid, provider: Arc::new(provider) })
    }

    /// 给 XCTest、WDA 等需要自己建连接的组件用
    pub fn provider(&self) -> Arc<dyn IdeviceProvider> {
        self.provider.clone()
    }

    /// usbmuxd 里有没有这台手机的配对记录。只有「没有记录」算 false；连不上 usbmuxd 等错误照常返回，
    /// 免得 usbmuxd 抖一下就让状态变成「请在手机上点信任」
    pub async fn paired(&self) -> Result<bool> {
        let mut mux = self.provider.addr.connect(0).await?;
        match mux.get_pair_record(&self.udid).await {
            Ok(_) => Ok(true),
            // 没有记录时 usbmuxd 的应答里没有 PairRecordData；记录损坏同样需要重新配对
            Err(IdeviceError::UnexpectedResponse(_)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// 发起配对：手机上弹「信任此电脑」。用户还没点时返回 [`DeviceError::PairingPending`]，调用方应轮询重试。
    /// 成功后配对记录交给 usbmuxd 保存（容器里即 `/var/lib/lockdown`）。
    pub async fn pair(&self, host_name: &str) -> Result<()> {
        let mut mux = self.provider.addr.connect(0).await?;
        let buid = mux.get_buid().await?;
        let mut lockdown = LockdownClient::connect(&*self.provider).await?;
        let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();
        let record = lockdown.pair_once(host_id, buid, Some(host_name)).await?;
        let mut mux = self.provider.addr.connect(0).await?;
        mux.save_pair_record(&self.udid, record.serialize()?).await?;
        Ok(())
    }

    /// 名称、型号、iOS 版本（不需要配对）
    pub async fn info(&self) -> Result<DeviceInfo> {
        let mut lockdown = LockdownClient::connect(&*self.provider).await?;
        let mut get = async |key: &str| -> String {
            match lockdown.get_value(Some(key), None).await {
                Ok(v) => v.as_string().map(str::to_owned).unwrap_or_default(),
                Err(e) => {
                    tracing::warn!("读设备信息 {key} 失败：{e}");
                    String::new()
                }
            }
        };
        Ok(DeviceInfo {
            udid: self.udid.clone(),
            name: get("DeviceName").await,
            product_type: get("ProductType").await,
            product_version: get("ProductVersion").await,
        })
    }

    pub async fn developer_mode_enabled(&self) -> Result<bool> {
        Ok(AmfiClient::connect(&*self.provider).await?.get_developer_mode_status().await?)
    }

    /// 让「设置 → 隐私与安全性」里出现开发者模式开关（打开仍需用户在手机上操作）
    pub async fn reveal_developer_mode(&self) -> Result<()> {
        Ok(AmfiClient::connect(&*self.provider).await?.reveal_developer_mode_option_in_ui().await?)
    }

    /// 建立 CoreDeviceProxy 用户态隧道并完成 RSD 握手。锁屏时报 [`DeviceError::PasswordProtected`]。
    pub async fn open_tunnel(&self) -> Result<Tunnel> {
        limit("建隧道", Duration::from_secs(30), async {
            let proxy = CoreDeviceProxy::connect(&*self.provider).await?;
            let rsd_port = proxy.tunnel_info().server_rsd_port;
            let mut adapter = proxy.create_software_tunnel()?.to_async_handle();
            let stream = adapter.connect(rsd_port).await?;
            let handshake = RsdHandshake::new(stream).await?;
            Ok(Tunnel { adapter, handshake, hid: None })
        })
        .await
    }

    /// 已安装的 App。`include_system` 为 false 只列用户 App，为 true 只列系统 App（与 toolkit 一致）。
    pub async fn list_apps(&self, include_system: bool) -> Result<Vec<AppInfo>> {
        let mut proxy = InstallationProxyClient::connect(&*self.provider).await?;
        let kind = if include_system { "System" } else { "User" };
        Ok(apps::parse_apps(proxy.get_apps(Some(kind), None).await?))
    }

    /// 经 usbmuxd 直连手机上的 TCP 端口（`iphone-control wda-bridge` 用）
    pub async fn connect_port(&self, port: u16) -> Result<Box<dyn idevice::ReadWrite>> {
        let dev = self.provider.connect(port).await?;
        dev.get_socket().ok_or_else(|| DeviceError::Idevice("连接没有套接字".into()))
    }

    /// 上传并安装 .ipa 或 .app 目录
    pub async fn install(&self, package: &Path) -> Result<()> {
        idevice::utils::installation::install_package(&*self.provider, package, None).await?;
        Ok(())
    }
}

/// 一条活着的隧道。HID、截图、cryptexd 都经它访问 RSD 服务；丢掉它隧道就断了。
///
/// HID 会话归隧道所有：它持有屏幕媒体流，必须在隧道断开之前停掉，否则手机端一直挂着这个会话
/// （灵动岛「配对的电脑」图标常驻、不会自动锁屏，进程退出和拔插数据线都清不掉）。
/// 所以不要直接丢掉 `Tunnel`，用 [`Tunnel::close`]。
pub struct Tunnel {
    adapter: AdapterHandle,
    handshake: RsdHandshake,
    hid: Option<HidSession>,
}

impl Tunnel {
    /// RSD 里是否提供某个服务（例如 iOS 27 的 `com.apple.coredevice.hid.universalhidservice`）
    pub fn has_service(&self, name: &str) -> bool {
        self.handshake.services.contains_key(name)
    }

    /// 已挂载的开发者镜像版本，未挂载返回 None
    pub async fn ddi_mounted(&mut self) -> Result<Option<String>> {
        limit("查询开发者镜像", Duration::from_secs(20), async {
            Ok(installed_ddi(&mut self.adapter, &mut self.handshake).await?.map(|c| c.version))
        })
        .await
    }

    /// 个性化并安装开发者镜像。`restore_dir` 见 [`ddi::ensure_cache`]
    pub async fn mount_ddi(&mut self, restore_dir: &Path) -> Result<String> {
        let assets = Cryptex1Assets::load(restore_dir).await?;
        // 含 TSS 联网签票
        limit("挂载开发者镜像", Duration::from_secs(180), async {
            Ok(install_ddi(&mut self.adapter, &mut self.handshake, &assets).await?.version)
        })
        .await
    }

    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        limit("截图", Duration::from_secs(30), async {
            let mut client = ScreenCaptureServiceClient::connect_rsd(&mut self.adapter, &mut self.handshake).await?;
            Ok(client.take_screenshot(None, ImageFormat::Png).await?)
        })
        .await
    }

    /// HID 会话；没有就开一个：先启动显示媒体流（HID 的授权闸门），再连 universalhidservice 和 indigo
    pub async fn hid(&mut self, screen: ScreenSize) -> Result<&mut HidSession> {
        if self.hid.is_none() {
            let h = limit("开启 HID 会话", Duration::from_secs(20), async {
                HidSession::open(&mut self.adapter, &mut self.handshake, screen).await
            })
            .await?;
            self.hid = Some(h);
        }
        Ok(self.hid.as_mut().expect("刚建好"))
    }

    pub fn hid_open(&self) -> bool {
        self.hid.is_some()
    }

    /// 停掉 HID 会话（含屏幕媒体流），隧道保留
    pub async fn close_hid(&mut self) {
        if let Some(h) = self.hid.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), h.close()).await;
        }
    }

    /// 关掉隧道：先停 HID 的屏幕媒体流
    pub async fn close(mut self) {
        self.close_hid().await;
    }
}
