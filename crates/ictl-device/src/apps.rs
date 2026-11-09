//! App 列表解析和按名字查 Bundle ID。纯函数，不访问设备。

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: String,
    /// CFBundleExecutable: the process name the app runs as (for filtering its logs)
    pub executable: String,
}

/// 解析 installation_proxy `Lookup` 的结果，按名字排序。
///
/// 带 `hidden` 标记的是系统后台服务（比如有 5 个也叫「设置」），去掉，免得按名字匹配到它们。
pub fn parse_apps(lookup: HashMap<String, plist::Value>) -> Vec<AppInfo> {
    let mut out: Vec<AppInfo> = lookup
        .into_iter()
        .filter_map(|(bid, info)| {
            let d = info.as_dictionary()?;
            let hidden = d
                .get("SBAppTags")
                .and_then(|t| t.as_array())
                .is_some_and(|tags| tags.iter().any(|t| t.as_string() == Some("hidden")));
            if hidden {
                return None;
            }
            let name = ["CFBundleDisplayName", "CFBundleName"]
                .iter()
                .find_map(|k| d.get(k).and_then(|v| v.as_string()).filter(|s| !s.is_empty()))
                .unwrap_or_default()
                .to_owned();
            let bundle_id = d.get("CFBundleIdentifier").and_then(|v| v.as_string()).unwrap_or(&bid).to_owned();
            let executable = d.get("CFBundleExecutable").and_then(|v| v.as_string()).unwrap_or_default().to_owned();
            Some(AppInfo { name, bundle_id, executable })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.bundle_id.cmp(&b.bundle_id)));
    out
}

/// 看起来就是 Bundle ID（带点、不带空格），不用查列表
pub fn looks_like_bundle_id(app: &str) -> bool {
    app.contains('.') && !app.contains(' ')
}

/// 在按优先级排列的 App 列表里，先找名字完全相同的，再找包含该文字的（不区分大小写）。
pub fn resolve<'a>(app: &str, lists: impl IntoIterator<Item = &'a [AppInfo]> + Clone) -> Option<String> {
    for apps in lists.clone() {
        if let Some(a) = apps.iter().find(|a| a.name == app) {
            return Some(a.bundle_id.clone());
        }
    }
    let needle = app.to_lowercase();
    for apps in lists {
        if let Some(a) = apps.iter().find(|a| a.name.to_lowercase().contains(&needle)) {
            return Some(a.bundle_id.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::{Dictionary, Value};

    fn app(name_key: &str, name: &str, bid: &str, hidden: bool) -> (String, Value) {
        let mut d = Dictionary::new();
        d.insert(name_key.into(), Value::String(name.into()));
        d.insert("CFBundleIdentifier".into(), Value::String(bid.into()));
        if hidden {
            d.insert("SBAppTags".into(), Value::Array(vec![Value::String("hidden".into())]));
        }
        (bid.to_owned(), Value::Dictionary(d))
    }

    fn system() -> Vec<AppInfo> {
        parse_apps(HashMap::from([
            app("CFBundleDisplayName", "设置", "com.apple.NewDeviceSetupUIService", true),
            app("CFBundleDisplayName", "设置", "com.apple.Preferences", false),
            app("CFBundleDisplayName", "设置", "com.apple.CarPlaySettings", true),
            app("CFBundleName", "时钟", "com.apple.mobiletimer", false),
            app("CFBundleDisplayName", "Safari浏览器", "com.apple.mobilesafari", false),
        ]))
    }

    fn user() -> Vec<AppInfo> {
        parse_apps(HashMap::from([
            app("CFBundleDisplayName", "开心消消乐", "com.example.game", false),
            app("CFBundleDisplayName", "设置助手", "com.example.settingshelper", false),
        ]))
    }

    #[test]
    fn hidden_system_services_are_skipped() {
        let s = system();
        assert!(s.contains(&AppInfo {
            name: "设置".into(),
            bundle_id: "com.apple.Preferences".into(),
            executable: String::new()
        }));
        assert!(s.iter().all(|a| a.bundle_id != "com.apple.NewDeviceSetupUIService"));
        assert_eq!(s.iter().filter(|a| a.name == "设置").count(), 1);
    }

    #[test]
    fn bundle_name_is_used_when_display_name_missing() {
        assert!(system().iter().any(|a| a.name == "时钟" && a.bundle_id == "com.apple.mobiletimer"));
    }

    #[test]
    fn empty_lookup() {
        assert!(parse_apps(HashMap::new()).is_empty());
    }

    #[test]
    fn exact_name_in_system_beats_substring_in_user() {
        let (u, s) = (user(), system());
        assert_eq!(resolve("设置", [&u[..], &s[..]]).as_deref(), Some("com.apple.Preferences"));
    }

    #[test]
    fn user_exact_match_wins() {
        let (u, s) = (user(), system());
        assert_eq!(resolve("开心消消乐", [&u[..], &s[..]]).as_deref(), Some("com.example.game"));
    }

    #[test]
    fn substring_match_is_case_insensitive() {
        let (u, s) = (user(), system());
        assert_eq!(resolve("safari", [&u[..], &s[..]]).as_deref(), Some("com.apple.mobilesafari"));
        assert_eq!(resolve("消消", [&u[..], &s[..]]).as_deref(), Some("com.example.game"));
    }

    #[test]
    fn bundle_id_detection_and_unknown() {
        assert!(looks_like_bundle_id("com.apple.Preferences"));
        assert!(!looks_like_bundle_id("Safari 浏览器"));
        let (u, s) = (user(), system());
        assert_eq!(resolve("不存在的 App", [&u[..], &s[..]]), None);
    }
}
