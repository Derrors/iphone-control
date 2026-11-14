//! Apple ID 登录、注册设备、描述文件、签名。
//!
//! 基于 vendor 的 Impactor 组件：`plume_core`（Apple 登录与开发者服务，MPL-2.0）、
//! `plume_store`（账号存储，MIT）、`omnisette`（anisette，MPL-2.0），以及打过权限补丁的 apple-codesign。
//! 签名流程本身在这里重写，修掉了 Impactor 漏签 `.xctest` 的问题（见 [`bundle`]）。
//!
//! 账号目录（数据目录下 `account/`，0700）：`accounts.json`（会话令牌）、`keys/<团队>/key.pem`（开发私钥）、anisette 状态。
//! 免费账号：描述文件 7 天过期、开发证书 1 年。
//!
//! 证书：每个账号目录有自己的私钥，对应团队里的一张开发证书；同一个团队同时最多两张（免费账号）。没有私钥（新的
//! 数据目录、换了机器）或私钥的证书已不在团队里时，签名会新建一张，满了就先吊销一张旧的，不挑是谁的。证书一被吊销，
//! 用它签的 App 在每台设备上都起不来，而描述文件看上去还有效。所以要能判断设备上的 WDA 是不是本机这把私钥签的
//! （[`AccountDir::local_public_keys`]、[`Profile::signed_with_any`]），本机的证书还在不在（[`Session::own_certificate_listed`]）。

pub mod bundle;
pub mod ipa;
pub mod profile;

use std::path::{Path, PathBuf};

use plume_core::{
    AnisetteConfiguration, CertificateIdentity, MobileProvision, SettingsScope, SigningSettings, UnifiedSigner,
    auth::Account as PlumeAccount, developer::DeveloperSession,
};
use plume_store::AccountStore;

pub use plume_core::auth::{TwoFactorAction, TwoFactorMethod, TwoFactorRequest};
pub use profile::Profile;
use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::pkcs8::DecodePrivateKey;

/// 证书（DER）里的公钥（SubjectPublicKeyInfo 的位串，RSA 时是 PKCS#1）。和 plume 认自己证书的比法一样
pub fn certificate_public_key(der: &[u8]) -> Option<Vec<u8>> {
    x509_certificate::X509Certificate::from_der(der).ok().map(|c| c.public_key_data().to_vec())
}

#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("{}", ictl_l10n::tr!("还没有登录 Apple ID：运行 iphone-control login", "Not signed in to an Apple ID yet: run iphone-control login"))]
    NotLoggedIn,
    #[error("{}", ictl_l10n::trf!("Apple ID 会话已失效，需要重新登录（iphone-control login）：{0}", "The Apple ID session expired; sign in again (iphone-control login): {0}", .0))]
    SessionInvalid(String),
    #[error("{}", ictl_l10n::tr!("账号下没有可用的开发团队", "The account has no usable development team"))]
    NoTeam,
    #[error("{}", ictl_l10n::trf!("签名失败：{0}", "Signing failed: {0}", .0))]
    Codesign(String),
    #[error("{}", ictl_l10n::trf!("开发者服务：{0}", "Developer services: {0}", .0))]
    Developer(String),
    #[error("{}", ictl_l10n::trf!("签名结果检查不通过：{0}", "The signed result failed verification: {0}", .0))]
    Verify(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<plume_core::Error> for SignError {
    fn from(e: plume_core::Error) -> Self {
        Self::Developer(e.to_string())
    }
}

impl From<plume_core::AppleCodesignError> for SignError {
    fn from(e: plume_core::AppleCodesignError) -> Self {
        Self::Codesign(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SignError>;

/// 账号目录及 anisette 设置
#[derive(Debug, Clone)]
pub struct AccountDir {
    pub dir: PathBuf,
    /// 自建 anisette 服务地址；None 用 Impactor 的默认（第三方）服务
    pub anisette_url: Option<String>,
}

impl AccountDir {
    fn store_path(&self) -> PathBuf {
        self.dir.join("accounts.json")
    }

    fn anisette(&self) -> AnisetteConfiguration {
        let c = AnisetteConfiguration::default().set_configuration_path(self.dir.clone());
        // 启用的是 v3 provider，它只看 v3 地址；原来设的是用不到的 v1 地址，自建服务不生效
        match &self.anisette_url {
            Some(url) => c.set_anisette_url_v3(url.clone()),
            None => c,
        }
    }

    /// 交互式登录。`two_factor` 在需要验证码时被调用（可在里面读终端输入）。成功后会话写入 `accounts.json`。
    pub async fn login(
        &self,
        email: &str,
        password: &str,
        two_factor: impl Fn(TwoFactorRequest) -> std::result::Result<TwoFactorAction, String>,
    ) -> Result<String> {
        restrict_dir(&self.dir)?;
        let (e, p) = (email.to_owned(), password.to_owned());
        let account = PlumeAccount::login(move || Ok((e.clone(), p.clone())), two_factor, self.anisette()).await?;
        let mut store =
            AccountStore::load(&Some(self.store_path())).await.map_err(|e| SignError::Developer(e.to_string()))?;
        store
            .accounts_add_from_session(email.to_owned(), account)
            .await
            .map_err(|e| SignError::Developer(e.to_string()))?;
        Ok(email.to_owned())
    }

    /// 本机签名私钥（`keys/<团队>/key.pem`）的公钥，PKCS#1 DER，可以和 [`certificate_public_key`] 比。还没签过名时为空
    pub fn local_public_keys(&self) -> Vec<Vec<u8>> {
        let Ok(dirs) = std::fs::read_dir(self.dir.join("keys")) else { return Vec::new() };
        dirs.flatten()
            .filter_map(|e| {
                let pem = std::fs::read_to_string(e.path().join("key.pem")).ok()?;
                let key = rsa::RsaPrivateKey::from_pkcs8_pem(&pem).ok()?;
                Some(key.to_public_key().to_pkcs1_der().ok()?.as_bytes().to_vec())
            })
            .collect()
    }

    /// 当前选中账号的邮箱
    pub async fn selected_email(&self) -> Result<String> {
        let store =
            AccountStore::load(&Some(self.store_path())).await.map_err(|e| SignError::Developer(e.to_string()))?;
        store.selected_account().map(|a| a.email().clone()).ok_or(SignError::NotLoggedIn)
    }

    /// 用保存的令牌恢复会话，取第一个团队
    pub async fn session(&self) -> Result<Session> {
        restrict_dir(&self.dir)?;
        restrict_tree(&self.dir)?;
        let store =
            AccountStore::load(&Some(self.store_path())).await.map_err(|e| SignError::Developer(e.to_string()))?;
        let acct = store.selected_account().ok_or(SignError::NotLoggedIn)?.clone();
        let inner = DeveloperSession::new(acct.adsid().clone(), acct.xcode_gs_token().clone(), self.anisette())
            .await
            .map_err(|e| SignError::SessionInvalid(e.to_string()))?;
        let teams = inner.qh_list_teams().await.map_err(|e| SignError::SessionInvalid(e.to_string()))?.teams;
        let team_id = teams.first().map(|t| t.team_id.clone()).ok_or(SignError::NoTeam)?;
        Ok(Session { inner, team_id, dir: self.clone() })
    }
}

/// 已登录的开发者会话
pub struct Session {
    inner: DeveloperSession,
    pub team_id: String,
    dir: AccountDir,
}

/// 一次签名的结果
#[derive(Debug, Clone)]
pub struct Signed {
    pub team_id: String,
    pub bundle_id: String,
    pub profile: Profile,
    /// 这次新建了签名证书（本机没有私钥，或私钥的证书已不在团队里）：别处用旧证书签的 App 不受影响，除非也吊销了
    pub new_certificate: bool,
    /// 新建时团队里的证书满了，吊销了一张旧的：用它签的 App 在所有设备上都起不来了，要重签
    pub revoked_certificate: bool,
}

impl Session {
    /// 本机的签名证书还在不在团队里（吊销了的不再列出）。本机还没有私钥时为 None
    pub async fn own_certificate_listed(&self) -> Result<Option<bool>> {
        let keys = self.dir.local_public_keys();
        if keys.is_empty() {
            return Ok(None);
        }
        let certs = self.inner.qh_list_certs(&self.team_id).await?.certificates;
        Ok(Some(
            certs
                .iter()
                .filter_map(|c| c.cert_content.as_ref())
                .filter_map(|d| certificate_public_key(d.as_ref()))
                .any(|k| keys.contains(&k)),
        ))
    }

    /// 把设备注册到团队（已注册则跳过）
    pub async fn ensure_device(&self, udid: &str, name: &str) -> Result<()> {
        self.inner.qh_ensure_device(&self.team_id, &name.to_owned(), &udid.to_owned()).await?;
        Ok(())
    }

    /// 签名 .ipa：Bundle ID 改成 `<原 ID>.<团队 ID>`（免费账号不能用别人的 ID），申请描述文件，深度优先签名所有嵌套 bundle。
    /// 签好后自检（每个嵌套 bundle 都有 `_CodeSignature`、描述文件含这台手机且带 get-task-allow）。
    pub async fn sign_ipa(&self, input: &Path, output: &Path, udid: &str) -> Result<Signed> {
        let work = std::env::temp_dir().join(format!("ictl-sign-{}", uuid::Uuid::new_v4()));
        let result = self.sign_in(&work, input, output, udid).await;
        let _ = std::fs::remove_dir_all(&work);
        result
    }

    async fn sign_in(&self, work: &Path, input: &Path, output: &Path, udid: &str) -> Result<Signed> {
        let app = ipa::unpack(input, work)?;
        let bundles = bundle::collect_sorted(&app)?;
        let main = bundles
            .last()
            .cloned()
            .ok_or_else(|| SignError::Codesign(ictl_l10n::tr!("空的 .app", "empty .app").into()))?;
        let orig = main.identifier().ok_or_else(|| {
            SignError::Codesign(
                ictl_l10n::tr!("主程序没有 CFBundleIdentifier", "the main executable has no CFBundleIdentifier").into(),
            )
        })?;
        let team = &self.team_id;
        let new_id = if orig.ends_with(&format!(".{team}")) { orig.clone() } else { format!("{orig}.{team}") };

        // 证书必须在申请描述文件之前定下来：本地没有私钥时这里会吊销旧证书、新建一张，
        // 而描述文件只包含生成那一刻团队里有效的证书，先拿描述文件会装不上（0xe8008015）。
        // 证书满了要吊销旧的之前 plume 调这个回调（返回 true 继续），记下来告诉调用方
        let mut revoked_certificate = false;
        let mut on_reset = || {
            revoked_certificate = true;
            true
        };
        let cert = CertificateIdentity::new_with_session(
            &self.inner,
            self.dir.dir.clone(),
            None,
            team,
            false,
            Some(&mut on_reset),
        )
        .await?;
        let new_certificate = cert.new;
        if new_certificate {
            tracing::warn!(
                "{}",
                ictl_l10n::log_tr!(
                    "新建了签名证书（本机没有私钥，或它的证书已不在团队里）",
                    "Created a new signing certificate (no local key, or its certificate is no longer in the team)"
                )
            );
        }

        // 只有带 entitlements 的 bundle（App/扩展）需要换 ID 并申请描述文件；WDA 里只有主程序一个
        let mut profiles = Vec::new();
        for b in bundles.iter().filter(|b| b.kind.has_entitlements()) {
            let id = b.identifier().unwrap_or_default().replace(&orig, &new_id);
            b.set_identifier(&id)?;
            let name = b.name().unwrap_or_else(|| id.clone());
            self.inner.qh_ensure_app_id(team, &name, &id).await?;
            let app_id = self.inner.qh_get_app_id(team, &id).await?.ok_or_else(|| {
                SignError::Developer(ictl_l10n::trf!(
                    "刚创建的 App ID {id} 查不到",
                    "The App ID {id} that was just created can't be found"
                ))
            })?;
            let data = self.inner.qh_get_profile(team, &app_id.app_id_id).await?.provisioning_profile.encoded_profile;
            let data: Vec<u8> = data.as_ref().to_vec();
            // 合并 entitlements 后在下面签名前写进 bundle
            profiles.push((b.dir.clone(), MobileProvision::load_with_bytes(data)?));
        }

        for b in bundles.iter().filter(|b| b.kind.should_be_signed()) {
            let mut settings = SigningSettings::default();
            cert.load_into_signing_settings(&mut settings)?;
            settings.set_for_notarization(false);
            settings.set_shallow(true);
            if let Some((_, prov)) = profiles.iter_mut().find(|(d, _)| *d == b.dir) {
                if let (Some(exe), Some(id)) = (b.executable(), b.identifier())
                    && let Err(e) = prov.merge_entitlements(b.dir.join(exe), &id)
                {
                    tracing::warn!(
                        "{}",
                        ictl_l10n::log_trf!(
                            "合并 {} 的 entitlements 失败，只用描述文件里的：{e}",
                            "Merging the entitlements of {} failed, using only those from the provisioning profile: {e}",
                            b.dir.display()
                        )
                    );
                }
                std::fs::write(b.dir.join("embedded.mobileprovision"), &prov.data)?;
                let xml = String::from_utf8_lossy(&prov.entitlements_as_bytes()?).into_owned();
                settings.set_entitlements_xml(SettingsScope::Main, xml)?;
            }
            tracing::info!("{}", ictl_l10n::log_trf!("签名 {}", "Signing {}", b.dir.display()));
            UnifiedSigner::new(settings).sign_path_in_place(&b.dir)?;
        }
        if let Some(key) = &cert.key {
            key.finish()?;
        }
        // 证书新建时 plume 按默认 umask 写私钥（0644），收紧
        restrict_tree(&self.dir.dir)?;

        let profile = verify_signed(&app, udid, cert.cert.as_ref().map(|c| c.constructed_data()))?;
        ipa::pack(work, output)?;
        Ok(Signed { team_id: team.clone(), bundle_id: new_id, profile, new_certificate, revoked_certificate })
    }
}

/// 签名后自检：每个要签名的嵌套 bundle 都要有 `_CodeSignature/CodeResources`（`.xctest` 漏签会在这里暴露），
/// 主程序的描述文件包含这台手机、带 get-task-allow（XCTest 要能附加调试）。
pub fn verify_signed(app_dir: &Path, udid: &str, signing_cert: Option<&[u8]>) -> Result<Profile> {
    for b in bundle::collect_sorted(app_dir)? {
        if matches!(b.kind, bundle::BundleType::Unknown | bundle::BundleType::Dylib) {
            continue;
        }
        if !b.dir.join("_CodeSignature/CodeResources").is_file() {
            return Err(SignError::Verify(ictl_l10n::trf!("{} 没有签名", "{} is not signed", b.dir.display())));
        }
    }
    let p = Profile::load(&app_dir.join("embedded.mobileprovision")).ok_or_else(|| {
        SignError::Verify(
            ictl_l10n::tr!(
                "主程序没有可解析的 embedded.mobileprovision",
                "the main executable has no readable embedded.mobileprovision"
            )
            .into(),
        )
    })?;
    if !p.devices.iter().any(|d| d == udid) {
        return Err(SignError::Verify(
            ictl_l10n::tr!(
                "描述文件里没有这台手机，先注册设备",
                "this phone is not in the provisioning profile; register the device first"
            )
            .into(),
        ));
    }
    if !p.get_task_allow {
        return Err(SignError::Verify(
            ictl_l10n::tr!(
                "描述文件不带 get-task-allow，WDA 起不来",
                "the provisioning profile lacks get-task-allow, WDA won't start"
            )
            .into(),
        ));
    }
    if let Some(der) = signing_cert
        && !p.certificates.iter().any(|c| c == der)
    {
        return Err(SignError::Verify(ictl_l10n::tr!("描述文件里没有签名用的证书（证书在描述文件生成后才换过），手机会拒装", "the provisioning profile doesn't contain the signing certificate (the certificate changed after the profile was created); the phone would refuse to install it").into()));
    }
    Ok(p)
}

fn restrict_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// 账号目录下的子目录 0700、文件 0600（里面有会话令牌和开发私钥）
fn restrict_tree(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o700))?;
            restrict_tree(&entry.path())?;
        } else if ft.is_file() {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 自建 anisette 服务地址要落到 v3 provider 实际读的字段上（原来设到 v1 字段，不生效）
    #[test]
    fn custom_anisette_url_reaches_v3_provider() {
        let dir = AccountDir { dir: PathBuf::from("/tmp/x"), anisette_url: Some("https://ani.example.com".into()) };
        assert_eq!(dir.anisette().anisette_url_v3(), "https://ani.example.com");
        let default = AccountDir { dir: PathBuf::from("/tmp/x"), anisette_url: None };
        assert_eq!(default.anisette().anisette_url_v3(), "https://mac-ani.jkcoxson.com");
    }
    use std::time::{Duration, SystemTime};

    #[test]
    fn account_tree_is_locked_down() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("ictl-acct-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("keys/TEAM")).unwrap();
        std::fs::write(root.join("keys/TEAM/key.pem"), b"k").unwrap();
        std::fs::set_permissions(root.join("keys/TEAM/key.pem"), std::fs::Permissions::from_mode(0o644)).unwrap();
        restrict_tree(&root).unwrap();
        let mode = |p: &str| std::fs::metadata(root.join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("keys/TEAM/key.pem"), 0o600);
        assert_eq!(mode("keys"), 0o700);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn signed_fake(root: &Path, sign_xctest: bool, devices: &[&str]) -> PathBuf {
        let app = bundle::tests::fake_wda(root);
        let mut dirs =
            vec![app.clone(), app.join("PlugIns/WebDriverAgentRunner.xctest/Frameworks/WebDriverAgentLib.framework")];
        if sign_xctest {
            dirs.push(app.join("PlugIns/WebDriverAgentRunner.xctest"));
        }
        for d in dirs {
            std::fs::create_dir_all(d.join("_CodeSignature")).unwrap();
            std::fs::write(d.join("_CodeSignature/CodeResources"), b"x").unwrap();
        }
        let exp = SystemTime::now() + Duration::from_secs(7 * 86400);
        std::fs::write(app.join("embedded.mobileprovision"), profile::tests::fake(exp, devices)).unwrap();
        app
    }

    #[test]
    fn verify_catches_unsigned_xctest() {
        let root = std::env::temp_dir().join(format!("ictl-verify-{}", uuid::Uuid::new_v4()));
        let app = signed_fake(&root, false, &["UDID-1"]);
        let err = verify_signed(&app, "UDID-1", None).unwrap_err().to_string();
        assert!(err.contains(".xctest"), "{err}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verify_passes_and_checks_device() {
        let root = std::env::temp_dir().join(format!("ictl-verify-{}", uuid::Uuid::new_v4()));
        let app = signed_fake(&root, true, &["UDID-1"]);
        assert!(verify_signed(&app, "UDID-1", Some(b"CERT-A")).is_ok());
        assert!(verify_signed(&app, "UDID-2", None).unwrap_err().to_string().contains("没有这台手机"));
        // 描述文件生成后证书被换掉（本地无私钥时 plume 会吊销重建）
        assert!(verify_signed(&app, "UDID-1", Some(b"CERT-B")).unwrap_err().to_string().contains("签名用的证书"));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 现场生成一把 RSA 私钥（PKCS#8 PEM）和用它自签的证书（DER）。dev 构建里大数运算开了优化（工作区 Cargo.toml），
    /// 生成 2048 位密钥不到一秒；仓库里不放私钥文件
    fn key_and_cert() -> (String, Vec<u8>) {
        use rsa::pkcs8::EncodePrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let pair =
            x509_certificate::InMemorySigningKeyPair::from_pkcs8_der(key.to_pkcs8_der().unwrap().as_bytes()).unwrap();
        let mut b = x509_certificate::X509CertificateBuilder::default();
        b.subject().append_common_name_utf8_string("ictl-sign test").unwrap();
        let cert = b.create_with_key_pair(&pair).unwrap().encode_der().unwrap();
        (key.to_pkcs8_pem(Default::default()).unwrap().to_string(), cert)
    }

    /// 设备上 WDA 的描述文件里有没有本机私钥的证书：iPad 上那份是别处签的（本机的证书是后来才建的），里面没有
    #[test]
    fn tells_whether_a_profile_has_this_machines_certificate() {
        let (pem, mine) = key_and_cert();
        let (_, other) = key_and_cert();
        let root = std::env::temp_dir().join(format!("ictl-keys-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("keys/TEAM1")).unwrap();
        std::fs::write(root.join("keys/TEAM1/key.pem"), pem).unwrap();
        let acct = AccountDir { dir: root.clone(), anisette_url: None };
        let keys = acct.local_public_keys();
        assert_eq!(keys.len(), 1);
        assert_eq!(certificate_public_key(&mine).as_ref(), Some(&keys[0]), "the same comparison plume uses");
        assert_ne!(certificate_public_key(&other).as_ref(), Some(&keys[0]));
        let profile = |certs: Vec<Vec<u8>>| Profile {
            expires: SystemTime::now(),
            devices: vec![],
            get_task_allow: true,
            team_ids: vec![],
            application_identifier: String::new(),
            certificates: certs,
        };
        assert!(!profile(vec![other.clone()]).signed_with_any(&keys), "signed elsewhere");
        assert!(profile(vec![other, mine]).signed_with_any(&keys), "lists every certificate valid at the time");
        assert!(!profile(vec![b"not a certificate".to_vec()]).signed_with_any(&keys));
        let none = AccountDir { dir: root.join("nothing"), anisette_url: None };
        assert!(none.local_public_keys().is_empty(), "never signed here");
        std::fs::remove_dir_all(root).unwrap();
    }
}
