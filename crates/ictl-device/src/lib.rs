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

use std::path::Path;
use std::sync::Arc;

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
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection},
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
    #[error("App 未安装：{0}")]
    AppNotInstalled(String),
    #[error("下载开发者镜像失败：{0}")]
    Download(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("idevice：{0}")]
    Idevice(String),
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
    let mut mux = UsbmuxdConnection::default().await?;
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

    /// usbmuxd 里有没有这台手机的配对记录
    pub async fn paired(&self) -> Result<bool> {
        let mut mux = self.provider.addr.connect(0).await?;
        Ok(mux.get_pair_record(&self.udid).await.is_ok())
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
            lockdown
                .get_value(Some(key), None)
                .await
                .ok()
                .and_then(|v| v.as_string().map(str::to_owned))
                .unwrap_or_default()
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
        let proxy = CoreDeviceProxy::connect(&*self.provider).await?;
        let rsd_port = proxy.tunnel_info().server_rsd_port;
        let mut adapter = proxy.create_software_tunnel()?.to_async_handle();
        let stream = adapter.connect(rsd_port).await?;
        let handshake = RsdHandshake::new(stream).await?;
        Ok(Tunnel { adapter, handshake })
    }

    /// 已安装的 App。`include_system` 为 false 只列用户 App，为 true 只列系统 App（与 toolkit 一致）。
    pub async fn list_apps(&self, include_system: bool) -> Result<Vec<AppInfo>> {
        let mut proxy = InstallationProxyClient::connect(&*self.provider).await?;
        let kind = if include_system { "System" } else { "User" };
        Ok(apps::parse_apps(proxy.get_apps(Some(kind), None).await?))
    }

    /// 经 usbmuxd 直连手机上的 TCP 端口（WDA bridge 用）
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
pub struct Tunnel {
    adapter: AdapterHandle,
    handshake: RsdHandshake,
}

impl Tunnel {
    /// RSD 里是否提供某个服务（例如 iOS 27 的 `com.apple.coredevice.hid.universalhidservice`）
    pub fn has_service(&self, name: &str) -> bool {
        self.handshake.services.contains_key(name)
    }

    /// 已挂载的开发者镜像版本，未挂载返回 None
    pub async fn ddi_mounted(&mut self) -> Result<Option<String>> {
        Ok(installed_ddi(&mut self.adapter, &mut self.handshake).await?.map(|c| c.version))
    }

    /// 诊断用：逐个调用 cryptexd 的只读例程，每个限时，返回 (例程, 结果) 列表
    pub async fn probe_cryptexd(&mut self, timeout: std::time::Duration) -> Vec<(&'static str, String)> {
        use idevice::{RsdService as _, cryptexd::CryptexdClient};
        let mut out = Vec::new();
        for routine in ["read-personalization-id", "copy-installed"] {
            let fut = async {
                let c: CryptexdClient<Box<dyn idevice::ReadWrite>> =
                    CryptexdClient::connect_rsd(&mut self.adapter, &mut self.handshake).await?;
                c.invoke(routine, Default::default()).await
            };
            let r = match tokio::time::timeout(timeout, fut).await {
                Err(_) => "超时".to_owned(),
                Ok(Err(e)) => format!("错误：{e}"),
                Ok(Ok(v)) => format!("成功：{}", format!("{v:?}").chars().take(160).collect::<String>()),
            };
            out.push((routine, r));
        }
        out
    }

    /// 诊断用：分步挂载开发者镜像（读标识 → 取 nonce → TSS 签票 → install），每步限时并打印耗时。
    /// 不调用 copy-installed，用于排查它挂住的情况（idevice 补丁前在 iOS 27.0.1 上出现过，见 spike/RESULTS.md）
    pub async fn mount_ddi_steps(&mut self, restore_dir: &Path, step_timeout: std::time::Duration) -> Result<()> {
        use idevice::{
            RsdService as _,
            cryptexd::{CryptexInstallRequest, CryptexdClient},
            tss::TSSRequest,
        };
        macro_rules! step {
            ($name:expr, $fut:expr) => {{
                let t0 = std::time::Instant::now();
                let r = tokio::time::timeout(step_timeout, $fut).await;
                let el = t0.elapsed().as_secs_f64();
                match r {
                    Err(_) => {
                        println!("  {}：超时（{el:.1}s）", $name);
                        return Err(DeviceError::Io(std::io::Error::other(format!("{} 超时", $name))));
                    }
                    Ok(Err(e)) => {
                        println!("  {}：失败（{el:.1}s）{e}", $name);
                        return Err(e.into());
                    }
                    Ok(Ok(v)) => {
                        println!("  {}：成功（{el:.1}s）", $name);
                        v
                    }
                }
            }};
        }
        let assets = Cryptex1Assets::load(restore_dir).await?;
        let (a, h) = (&mut self.adapter, &mut self.handshake);
        let chip = step!("读个性化标识", async {
            let c: CryptexdClient<Box<dyn idevice::ReadWrite>> = CryptexdClient::connect_rsd(a, h).await?;
            c.read_personalization_identifiers().await
        });
        let domain = assets.nonce_domain()?;
        let (a, h) = (&mut self.adapter, &mut self.handshake);
        let nonce = step!("取 cryptex nonce", async {
            let c: CryptexdClient<Box<dyn idevice::ReadWrite>> = CryptexdClient::connect_rsd(a, h).await?;
            c.cryptex_nonce(domain).await
        });
        let mut req = TSSRequest::new();
        req.add_cryptex1_tags(&assets.build_identity, &chip, &nonce)?;
        let resp = step!("TSS 签票", req.send());
        let resp = resp.into_dictionary().ok_or_else(|| DeviceError::Io(std::io::Error::other("TSS 响应不是字典")))?;
        let key = if resp.contains_key("Cryptex1,Ticket") { "Cryptex1,Ticket" } else { "ApImg4Ticket" };
        println!("  票据字段：{key}");
        let im4m = resp
            .get(key)
            .and_then(|v| v.as_data())
            .ok_or_else(|| DeviceError::Io(std::io::Error::other("TSS 响应里没有票据")))?
            .to_vec();
        let request = CryptexInstallRequest::new(
            assets.image.clone(),
            assets.trustcache.clone(),
            im4m,
            assets.info.clone(),
            assets.volumehash.clone(),
            assets.cryptex1_properties()?,
        );
        let (a, h) = (&mut self.adapter, &mut self.handshake);
        step!("install", async {
            let c: CryptexdClient<Box<dyn idevice::ReadWrite>> = CryptexdClient::connect_rsd(a, h).await?;
            c.install(request).await
        });
        Ok(())
    }

    /// 个性化并安装开发者镜像。`restore_dir` 见 [`ddi::ensure_cache`]
    pub async fn mount_ddi(&mut self, restore_dir: &Path) -> Result<String> {
        let assets = Cryptex1Assets::load(restore_dir).await?;
        Ok(install_ddi(&mut self.adapter, &mut self.handshake, &assets).await?.version)
    }

    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        let mut client = ScreenCaptureServiceClient::connect_rsd(&mut self.adapter, &mut self.handshake).await?;
        Ok(client.take_screenshot(None, ImageFormat::Png).await?)
    }

    /// 开启 HID 会话：先启动显示媒体流（HID 的授权闸门），再连 universalhidservice 和 indigo
    pub async fn hid(&mut self, screen: ScreenSize) -> Result<HidSession> {
        HidSession::open(&mut self.adapter, &mut self.handshake, screen).await
    }
}
