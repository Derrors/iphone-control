//! CoreDevice requests the native path (iOS 27, no WebDriverAgent) needs: launching an app in front, optionally
//! with a link for it to open; sending a process a signal; whether the phone is locked.

use std::time::Duration;

use idevice::core_device::{AppServiceClient, CoreDeviceServiceClient};
use idevice::lockdown::LockdownClient;
use idevice::{IdeviceError, IdeviceService, ReadWrite, RsdService};
use plist::{Dictionary, Value};

use crate::{Device, DeviceError, Result, Tunnel, limit};

/// AppService through the generic CoreDevice client: idevice's `launch_application` has no payload URL
struct AppService(CoreDeviceServiceClient<Box<dyn ReadWrite>>);

impl RsdService for AppService {
    fn rsd_service_name() -> std::borrow::Cow<'static, str> {
        "com.apple.coredevice.appservice".into()
    }

    async fn from_stream(stream: Box<dyn ReadWrite>) -> std::result::Result<Self, IdeviceError> {
        Ok(Self(CoreDeviceServiceClient::new(stream).await?))
    }
}

/// The launch request (`com.apple.coredevice.feature.launchapplication`), with `url` as the payload URL when given
/// (as `devicectl device process launch --payload-url`)
pub(crate) fn launch_request(bundle_id: &str, url: Option<&str>) -> Dictionary {
    let mut platform = Vec::new();
    // an empty dictionary into memory: cannot fail
    plist::to_writer_binary(&mut platform, &Value::Dictionary(Dictionary::new())).expect("plist into memory");
    let mut user = Dictionary::new();
    user.insert("active".into(), true.into());
    let mut options = Dictionary::new();
    options.insert("arguments".into(), Value::Array(Vec::new()));
    options.insert("environmentVariables".into(), Value::Dictionary(Dictionary::new()));
    options.insert("platformSpecificOptions".into(), Value::Data(platform));
    options.insert("standardIOUsesPseudoterminals".into(), true.into());
    options.insert("startStopped".into(), false.into());
    options.insert("terminateExisting".into(), false.into());
    options.insert("user".into(), Value::Dictionary(user));
    if let Some(u) = url {
        let mut payload = Dictionary::new();
        payload.insert("relative".into(), u.into());
        options.insert("payloadURL".into(), Value::Dictionary(payload));
    }
    let mut id = Dictionary::new();
    id.insert("_0".into(), bundle_id.into());
    let mut spec = Dictionary::new();
    spec.insert("bundleIdentifier".into(), Value::Dictionary(id));
    let mut req = Dictionary::new();
    req.insert("applicationSpecifier".into(), Value::Dictionary(spec));
    req.insert("options".into(), Value::Dictionary(options));
    req.insert("standardIOIdentifiers".into(), Value::Dictionary(Dictionary::new()));
    req
}

impl Tunnel {
    /// Launches an app in front (a running one comes to the front without restarting), with `url` for it to open;
    /// returns its process. SpringBoard refuses while the phone is locked
    pub async fn launch_foreground(&mut self, bundle_id: &str, url: Option<&str>) -> Result<u64> {
        limit(ictl_l10n::text!("启动 App", "launch the app"), Duration::from_secs(30), async {
            let mut c = AppService::connect_rsd(&mut self.adapter, &mut self.handshake).await?;
            let res =
                c.0.invoke_with_plist("com.apple.coredevice.feature.launchapplication", launch_request(bundle_id, url))
                    .await?;
            res.as_dictionary()
                .and_then(|d| d.get("processToken"))
                .and_then(Value::as_dictionary)
                .and_then(|t| t.get("processIdentifier"))
                .and_then(Value::as_signed_integer)
                .map(|p| p as u64)
                .ok_or_else(|| DeviceError::Idevice(format!("launching {bundle_id}: no process in the answer")))
        })
        .await
    }

    /// Sends `pid` a signal (9 ends it) through CoreDevice AppService
    pub async fn signal(&mut self, pid: u64, signal: u32) -> Result<()> {
        limit(ictl_l10n::text!("结束进程", "end a process"), Duration::from_secs(20), async {
            let mut c = AppServiceClient::connect_rsd(&mut self.adapter, &mut self.handshake).await?;
            c.send_signal(pid as u32, signal).await?;
            Ok(())
        })
        .await
    }
}

impl Device {
    /// Whether the phone is locked: lockdown's `PasswordProtected`, true while it needs its passcode (iOS 27's
    /// developer disk image doesn't implement CoreDevice's lock state). A phone without a passcode never reads as
    /// locked
    pub async fn passcode_locked(&self) -> Result<bool> {
        limit(ictl_l10n::text!("查锁屏状态", "check the lock state"), Duration::from_secs(5), async {
            let mut lockdown = LockdownClient::connect(&*self.provider).await?;
            // asked without a session, lockdown answers "this request was prohibited"
            let pairing = idevice::provider::IdeviceProvider::get_pairing_file(&*self.provider).await?;
            lockdown.start_session(&pairing).await?;
            let v = lockdown.get_value(Some("PasswordProtected"), None).await?;
            v.as_boolean().ok_or_else(|| DeviceError::Idevice(format!("PasswordProtected: {v:?}")))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_rides_in_the_launch_request() {
        let r = launch_request("com.apple.mobilesafari", Some("https://example.com/a?b=1"));
        let opts = r["options"].as_dictionary().unwrap();
        assert_eq!(
            opts["payloadURL"].as_dictionary().unwrap()["relative"].as_string(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(opts["terminateExisting"].as_boolean(), Some(false));
        let spec = r["applicationSpecifier"].as_dictionary().unwrap()["bundleIdentifier"].as_dictionary().unwrap();
        assert_eq!(spec["_0"].as_string(), Some("com.apple.mobilesafari"));
        assert!(!launch_request("x", None)["options"].as_dictionary().unwrap().contains_key("payloadURL"));
    }
}
