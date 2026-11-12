//! 描述文件（.mobileprovision）解析。纯函数。
//!
//! 文件是 CMS 签名包着一个 XML plist，直接截取 `<?xml` 到 `</plist>` 那段解析即可（不验签）。

use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub expires: SystemTime,
    pub devices: Vec<String>,
    pub get_task_allow: bool,
    /// The signing team IDs in TeamIdentifier. A legacy App ID prefix can differ from these.
    pub team_ids: Vec<String>,
    /// App ID prefix followed by the bundle identifier.
    pub application_identifier: String,
    /// 允许签名的证书（DER）。签名用的证书不在其中时手机拒装（0xe8008015）
    pub certificates: Vec<Vec<u8>>,
}

impl Profile {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let start = find(data, b"<?xml")?;
        let end = find(&data[start..], b"</plist>")? + start + b"</plist>".len();
        let v: plist::Value = plist::from_bytes(&data[start..end]).ok()?;
        let d = v.as_dictionary()?;
        let ent = d.get("Entitlements").and_then(|e| e.as_dictionary());
        Some(Self {
            expires: d.get("ExpirationDate").and_then(|v| v.as_date()).map(SystemTime::from)?,
            devices: d
                .get("ProvisionedDevices")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_string().map(str::to_owned)).collect())
                .unwrap_or_default(),
            get_task_allow: ent.and_then(|e| e.get("get-task-allow")).and_then(|v| v.as_boolean()).unwrap_or(false),
            team_ids: d
                .get("TeamIdentifier")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_string().map(str::to_owned)).collect())
                .unwrap_or_default(),
            application_identifier: ent
                .and_then(|e| e.get("application-identifier"))
                .and_then(|v| v.as_string())
                .unwrap_or_default()
                .to_owned(),
            certificates: d
                .get("DeveloperCertificates")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_data().map(<[u8]>::to_vec)).collect())
                .unwrap_or_default(),
        })
    }

    pub fn load(path: &std::path::Path) -> Option<Self> {
        Self::parse(&std::fs::read(path).ok()?)
    }

    /// 描述文件里有没有这些公钥（[`crate::AccountDir::local_public_keys`]）之一的证书。描述文件列的是生成那一刻
    /// 团队里所有有效的证书，所以「有」不等于是用它签的；「没有」则一定不是本机签的（别的机器、旧的数据目录签的，
    /// 或本机的证书是后来才新建的）
    pub fn signed_with_any(&self, keys: &[Vec<u8>]) -> bool {
        self.certificates.iter().filter_map(|c| crate::certificate_public_key(c)).any(|k| keys.contains(&k))
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

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
        d.insert("DeveloperCertificates".into(), plist::Value::Array(vec![plist::Value::Data(b"CERT-A".to_vec())]));
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
        assert_eq!(p.devices, ["00000000-AAAA"]);
        assert!(p.get_task_allow);
        assert_eq!(p.expires, exp);
        assert_eq!(p.team_ids, ["TEAM000000"]);
        assert!(p.application_identifier.starts_with("TEAM000000."));
        assert_eq!(p.certificates, [b"CERT-A".to_vec()]);
    }

    #[test]
    fn garbage_is_none() {
        assert!(Profile::parse(b"not a profile").is_none());
    }
}
