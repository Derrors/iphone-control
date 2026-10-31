//! 启动手机上的 WebDriverAgent（XCTest runner）。
//!
//! 用 idevice 的 `XCUITestService` 跑测试计划；它自己经 usbmuxd 建 CoreDeviceProxy 隧道，和设备层的隧道互不影响。
//! 就绪判定 = 本进程持有的 runner 任务还活着 **且** `/status` ready（手机上残留的 WDA 在 runner 退出后还会响应几十秒）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use idevice::{
    IdeviceError, IdeviceService,
    dvt::xctest::{TestConfig, XCUITestService, listener::XCUITestListener},
    installation_proxy::InstallationProxyClient,
    provider::IdeviceProvider,
};
use tokio::task::JoinHandle;

use crate::http::DeviceTransport;
use crate::{Result, WdaClient, WdaError, is_automation_pin_error};

pub const WDA_PORT: u16 = 8100;

/// 记下 XCTest 过程中报告的错误文字，用来识别「Enable UI Automation」密码框超时
struct ErrorListener(Arc<Mutex<Vec<String>>>);

impl ErrorListener {
    fn push(&self, s: &str) {
        self.0.lock().unwrap().push(s.to_owned());
    }
}

impl XCUITestListener for ErrorListener {
    async fn initialization_for_ui_testing_did_fail(&mut self, description: &str) -> std::result::Result<(), IdeviceError> {
        self.push(description);
        Ok(())
    }

    async fn did_fail_to_bootstrap(&mut self, description: &str) -> std::result::Result<(), IdeviceError> {
        self.push(description);
        Err(IdeviceError::UnexpectedResponse(format!("test runner failed to bootstrap: {description}")))
    }
}

pub struct WdaRunner {
    pub bundle_id: String,
    task: JoinHandle<std::result::Result<(), IdeviceError>>,
}

impl WdaRunner {
    /// 启动 runner 并等到 `/status` ready。`bundle_id` 是签名后带团队 ID 的 xctrunner Bundle ID。
    pub async fn start(provider: Arc<dyn IdeviceProvider>, bundle_id: &str, timeout: Duration) -> Result<(Self, WdaClient<DeviceTransport>)> {
        let mut ip = InstallationProxyClient::connect(&*provider).await.map_err(|e| WdaError::Unavailable(e.to_string()))?;
        let cfg = TestConfig::from_installation_proxy(&mut ip, bundle_id, None).await.map_err(|e| match e {
            IdeviceError::AppNotInstalled => WdaError::NotInstalled(bundle_id.to_owned()),
            other => WdaError::StartFailed(other.to_string()),
        })?;

        let errors = Arc::new(Mutex::new(Vec::new()));
        let task = {
            let (provider, errors) = (provider.clone(), errors.clone());
            tokio::spawn(async move { XCUITestService::new(provider).run(cfg, &mut ErrorListener(errors), None).await })
        };
        let runner = Self { bundle_id: bundle_id.to_owned(), task };
        let client = WdaClient::new(DeviceTransport::new(provider, WDA_PORT));

        let failure = |extra: Option<String>| {
            let mut text: Vec<String> = errors.lock().unwrap().clone();
            text.extend(extra);
            let joined = text.join("; ");
            if is_automation_pin_error(&joined) { WdaError::NeedsAutomationPin } else { WdaError::StartFailed(joined) }
        };

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if runner.task.is_finished() {
                let r = runner.task.await;
                let msg = match r {
                    Ok(Ok(())) => "runner 提前结束".to_owned(),
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                };
                return Err(failure(Some(msg)));
            }
            if client.ready().await.unwrap_or(false) {
                return Ok((runner, client));
            }
            if tokio::time::Instant::now() >= deadline {
                runner.task.abort();
                return Err(failure(Some(format!("{} 秒内 WDA 没有就绪", timeout.as_secs()))));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// runner 任务是否还活着。死了就算手机上的 WDA 还在响应也不能用
    pub fn alive(&self) -> bool {
        !self.task.is_finished()
    }

    pub fn stop(self) {
        self.task.abort();
    }
}
