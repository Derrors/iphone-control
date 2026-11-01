//! 描述文件（.mobileprovision）解析。纯函数。
//!
//! 文件是 CMS 签名包着一个 XML plist，直接截取 `<?xml` 到 `</plist>` 那段解析即可（不验签）。

use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub team_id: String,
    pub expires: SystemTime,
    pub devices: Vec<String>,
    pub get_task_allow: bool,
    /// 形如 `TEAMID.com.example.app`
    pub application_identifier: String,
}

impl Profile {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let start = find(data, b"<?xml")?;
        let end = find(&data[start..], b"</plist>")? + start + b"</plist>".len();
        let v: plist::Value = plist::from_bytes(&data[start..end]).ok()?;
        let d = v.as_dictionary()?;
        let ent = d.get("Entitlements").and_then(|e| e.as_dictionary());
        Some(Self {
            name: d.get("Name").and_then(|v| v.as_string()).unwrap_or_default().to_owned(),
            team_id: d
                .get("TeamIdentifier")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_string())
                .unwrap_or_default()
                .to_owned(),
            expires: d.get("ExpirationDate").and_then(|v| v.as_date()).map(SystemTime::from)?,
            devices: d
                .get("ProvisionedDevices")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_string().map(str::to_owned)).collect())
                .unwrap_or_default(),
            get_task_allow: ent.and_then(|e| e.get("get-task-allow")).and_then(|v| v.as_boolean()).unwrap_or(false),
            application_identifier: ent
                .and_then(|e| e.get("application-identifier"))
                .and_then(|v| v.as_string())
                .unwrap_or_default()
                .to_owned(),
        })
    }

    pub fn load(path: &std::path::Path) -> Option<Self> {
        Self::parse(&std::fs::read(path).ok()?)
    }

    /// 还剩多久过期；已过期返回 0
    pub fn remaining(&self, now: SystemTime) -> Duration {
        self.expires.duration_since(now).unwrap_or_default()
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 造一个假的描述文件：前后各垫一段二进制，模拟 CMS 外壳
    pub fn fake(expires: SystemTime, devices: &[&str]) -> Vec<u8> {
        let mut ent = plist::Dictionary::new();
        ent.insert("get-task-allow".into(), true.into());
        ent.insert("application-identifier".into(), "TEAM000000.com.example.app.TEAM000000".into());
        let mut d = plist::Dictionary::new();
        d.insert("Name".into(), "iOS Team Provisioning Profile: test".into());
        d.insert("TeamIdentifier".into(), plist::Value::Array(vec!["TEAM000000".into()]));
        d.insert("ExpirationDate".into(), plist::Value::Date(expires.into()));
        d.insert("ProvisionedDevices".into(), plist::Value::Array(devices.iter().map(|s| (*s).into()).collect()));
        d.insert("Entitlements".into(), plist::Value::Dictionary(ent));
        let mut xml = Vec::new();
        plist::Value::Dictionary(d).to_writer_xml(&mut xml).unwrap();
        let mut out = vec![0x30, 0x82, 0x01, 0x00, 0xfa, 0xde];
        out.extend(xml);
        out.extend([0x00, 0xa0, 0x82]);
        out
    }

    #[test]
    fn parses_profile_inside_cms_wrapper() {
        let exp = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let p = Profile::parse(&fake(exp, &["00000000-AAAA"])).unwrap();
        assert_eq!(p.team_id, "TEAM000000");
        assert_eq!(p.devices, ["00000000-AAAA"]);
        assert!(p.get_task_allow);
        assert_eq!(p.expires, exp);
        assert!(p.application_identifier.starts_with("TEAM000000."));
    }

    #[test]
    fn remaining_saturates_at_zero() {
        let exp = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let p = Profile::parse(&fake(exp, &[])).unwrap();
        assert_eq!(p.remaining(exp + Duration::from_secs(5)), Duration::ZERO);
        assert_eq!(p.remaining(exp - Duration::from_secs(5)), Duration::from_secs(5));
    }

    #[test]
    fn garbage_is_none() {
        assert!(Profile::parse(b"not a profile").is_none());
    }
}
