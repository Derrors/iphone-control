//! Apple ID 登录、注册设备、描述文件、签名。
//!
//! 阶段 4 会 vendor Impactor 的 `plume_core`（MPL-2.0）、`plume_utils`、`omnisette`，并打两处补丁：
//! 1. `plume_utils::BundleType` 增加 `XCTest`：`.xctest` 要签名、不带 entitlements。
//!    原实现把 `xctest` 归为 `Unknown` 直接跳过，导致 WDA 报 XCTestErrorDomain 103。
//! 2. apple-codesign 0.29.0 的权限补丁（Impactor `patches/apple-codesign-0.29.0/0001-apply-0o755-perms.patch`）。
//!    作为 git 依赖引用 Impactor 时它的 `[patch.crates-io]` 不生效，所以必须 vendor。
//!
//! 免费账号：描述文件 7 天过期、开发证书 1 年；anisette 服务地址可配置（默认第三方）。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("Apple ID 会话已失效，需要重新登录")]
    SessionExpired,
    #[error("需要双重认证验证码")]
    TwoFactorRequired,
    #[error("签名失败：{0}")]
    Codesign(String),
    #[error("开发者服务：{0}")]
    Developer(String),
}

pub type Result<T> = std::result::Result<T, SignError>;

/// 持久化的账号状态（会话、团队、私钥），存放在数据目录 `account/`，权限 0700
pub struct Account {
    pub team_id: String,
    pub dir: PathBuf,
}

/// 双重认证验证码的获取方式（CLI 交互输入）
pub trait TwoFactorPrompt {
    fn code(&mut self) -> String;
}

impl Account {
    pub async fn login(dir: &Path, apple_id: &str, password: &str, prompt: &mut dyn TwoFactorPrompt) -> Result<Self> {
        let _ = (dir, apple_id, password, prompt);
        todo!("plume_core::auth::account + omnisette")
    }

    pub async fn load(dir: &Path) -> Result<Self> {
        let _ = dir;
        todo!()
    }

    /// 每天调用一次，提前发现会话过期（不要等到重签时）
    pub async fn verify_session(&self) -> Result<()> {
        todo!("developer::qh::teams")
    }

    pub async fn register_device(&self, udid: &str, name: &str) -> Result<()> {
        let _ = (udid, name);
        todo!("developer::qh::devices")
    }

    /// 为 bundle id 取一份新的开发描述文件（会注册 App ID）
    pub async fn provisioning_profile(&self, bundle_id: &str) -> Result<Profile> {
        let _ = bundle_id;
        todo!("developer::qh::{{app_ids, profile}}")
    }
}

pub struct Profile {
    pub path: PathBuf,
    pub expires: SystemTime,
    pub devices: Vec<String>,
    pub get_task_allow: bool,
}

/// 签名 .ipa/.app：深度优先，包括 Frameworks、PlugIns/*.appex、PlugIns/*.xctest
pub async fn sign(input: &Path, output: &Path, account: &Account, profile: &Profile, bundle_id: &str) -> Result<()> {
    let _ = (input, output, account, profile, bundle_id);
    todo!("plume_utils::Signer（打过 XCTest 补丁）")
}

/// 签名后自检：每个嵌套 bundle 都要有 `_CodeSignature`，主程序带 get-task-allow
pub fn verify_signed(app_dir: &Path) -> Result<()> {
    let _ = app_dir;
    todo!()
}
