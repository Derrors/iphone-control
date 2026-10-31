//! 找出一个 .app 里所有要签名的嵌套 bundle。
//!
//! 改写自 Impactor v2.7.2 `crates/plume_utils/src/bundle.rs`（MIT，Copyright 2025 Samara M）。
//! 关键改动：增加 [`BundleType::XCTest`]。原实现把 `.xctest` 归为 `Unknown`、签名时直接跳过，
//! 结果 WebDriverAgent 的测试插件没有签名，手机上报 XCTestErrorDomain 103。

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleType {
    App,
    AppExtension,
    XCTest,
    Framework,
    Dylib,
    Unknown,
}

impl BundleType {
    pub fn from_extension(ext: &str) -> Self {
        match ext {
            "app" => Self::App,
            "appex" => Self::AppExtension,
            "xctest" => Self::XCTest,
            "framework" => Self::Framework,
            "dylib" => Self::Dylib,
            _ => Self::Unknown,
        }
    }

    /// 带描述文件里的 entitlements 签名的只有 App 和 App 扩展；
    /// XCTest、框架、动态库签名但不带 entitlements（与 go-ios 签出、实测能启动的 WDA 一致）
    pub fn has_entitlements(self) -> bool {
        matches!(self, Self::App | Self::AppExtension)
    }

    pub fn should_be_signed(self) -> bool {
        self != Self::Unknown
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub dir: PathBuf,
    pub kind: BundleType,
}

impl Bundle {
    pub fn info_plist(&self) -> PathBuf {
        self.dir.join("Info.plist")
    }

    fn read_string(&self, key: &str) -> Option<String> {
        plist::Value::from_file(self.info_plist())
            .ok()?
            .as_dictionary()?
            .get(key)?
            .as_string()
            .map(str::to_owned)
    }

    pub fn identifier(&self) -> Option<String> {
        self.read_string("CFBundleIdentifier")
    }

    pub fn executable(&self) -> Option<String> {
        self.read_string("CFBundleExecutable")
    }

    pub fn name(&self) -> Option<String> {
        self.read_string("CFBundleName").or_else(|| self.read_string("CFBundleDisplayName"))
    }

    pub fn set_identifier(&self, id: &str) -> std::io::Result<()> {
        let mut v = plist::Value::from_file(self.info_plist()).map_err(std::io::Error::other)?;
        if let Some(d) = v.as_dictionary_mut() {
            d.insert("CFBundleIdentifier".into(), id.into());
        }
        v.to_file_xml(self.info_plist()).map_err(std::io::Error::other)
    }
}

fn is_macho(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    let ok = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok();
    let (be, le) = (u32::from_be_bytes(magic), u32::from_le_bytes(magic));
    // MH_MAGIC、MH_MAGIC_64、FAT_MAGIC
    ok && [0xfeedface, 0xfeedfacf, 0xcafebabe].iter().any(|m| *m == be || *m == le)
}

fn collect(dir: &Path, out: &mut Vec<Bundle>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        if path.is_symlink() {
            continue;
        }
        if path.is_file() {
            if (name.ends_with(".dylib") || name.ends_with(".so")) && is_macho(&path) {
                out.push(Bundle { dir: path, kind: BundleType::Dylib });
            }
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        if let Some((_, ext)) = name.rsplit_once('.')
            && path.join("Info.plist").is_file()
        {
            let kind = BundleType::from_extension(ext);
            out.push(Bundle { dir: path.clone(), kind });
            // 嵌套的 .app 自己会被单独当成顶层处理，不往里钻
            if kind != BundleType::App {
                collect(&path, out)?;
            }
            continue;
        }
        collect(&path, out)?;
    }
    Ok(())
}

/// 包括 `app_dir` 自己在内的所有 bundle，按路径深度从深到浅排列：里层先签，外层封存时才能拿到里层的签名
pub fn collect_sorted(app_dir: &Path) -> std::io::Result<Vec<Bundle>> {
    let mut out = Vec::new();
    collect(app_dir, &mut out)?;
    let ext = app_dir.extension().and_then(|e| e.to_str()).unwrap_or("app");
    out.push(Bundle { dir: app_dir.to_owned(), kind: BundleType::from_extension(ext) });
    out.sort_by_key(|b| std::cmp::Reverse(b.dir.components().count()));
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 造一个 WDA 结构的假 .app：主程序、PlugIns/X.xctest、里面的 Frameworks/Lib.framework、一个 .lproj 资源目录
    pub fn fake_wda(root: &Path) -> PathBuf {
        let app = root.join("Payload/WebDriverAgentRunner-Runner.app");
        let xctest = app.join("PlugIns/WebDriverAgentRunner.xctest");
        let fw = xctest.join("Frameworks/WebDriverAgentLib.framework");
        for d in [&app, &xctest, &fw, &app.join("en.lproj")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let plist = |dir: &Path, id: &str, exe: &str| {
            let mut d = plist::Dictionary::new();
            d.insert("CFBundleIdentifier".into(), id.into());
            d.insert("CFBundleExecutable".into(), exe.into());
            plist::Value::Dictionary(d).to_file_xml(dir.join("Info.plist")).unwrap();
        };
        plist(&app, "com.facebook.WebDriverAgentRunner.xctrunner", "WebDriverAgentRunner-Runner");
        plist(&xctest, "com.facebook.WebDriverAgentRunner", "WebDriverAgentRunner");
        plist(&fw, "com.facebook.WebDriverAgentLib", "WebDriverAgentLib");
        std::fs::write(app.join("en.lproj/InfoPlist.strings"), b"x").unwrap();
        app
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ictl-sign-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn xctest_is_collected_and_signed_without_entitlements() {
        let root = tmp("bundle");
        let app = fake_wda(&root);
        let bundles = collect_sorted(&app).unwrap();
        let kinds: Vec<_> = bundles.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, [BundleType::Framework, BundleType::XCTest, BundleType::App]);
        assert!(BundleType::XCTest.should_be_signed());
        assert!(!BundleType::XCTest.has_entitlements());
        assert!(BundleType::App.has_entitlements());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directories_without_info_plist_are_not_bundles() {
        let root = tmp("lproj");
        let app = fake_wda(&root);
        assert!(collect_sorted(&app).unwrap().iter().all(|b| !b.dir.ends_with("en.lproj")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_extension_is_not_signed() {
        assert_eq!(BundleType::from_extension("bundle"), BundleType::Unknown);
        assert!(!BundleType::Unknown.should_be_signed());
    }
}
