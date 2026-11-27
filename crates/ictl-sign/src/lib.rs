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
use std::time::Duration;

use plume_core::{
    AnisetteConfiguration, CertificateIdentity, MobileProvision, SettingsScope, SigningSettings, UnifiedSigner,
    auth::Account as PlumeAccount, developer::DeveloperSession,
};
use plume_store::AccountStore;

pub use plume_core::auth::{TwoFactorAction, TwoFactorMethod, TwoFactorRequest};
pub use profile::Profile;
use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::pkcs8::DecodePrivateKey;

/// Backoff delays before retry attempt 2 and attempt 3 when Anisette or Apple's service hits a transient failure.
pub const SESSION_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];
/// Per-attempt timeout when fetching Anisette headers or establishing a developer session, bounded so up to 3 attempts
/// plus backoff still finish within the daemon's 3-minute session-check timeout.
pub const SESSION_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(45);

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
    #[error("{}", ictl_l10n::trf!("暂时连不上 Anisette 或苹果服务（可稍后重试）：{0}", "Temporarily unable to reach the Anisette or Apple service (retry later): {0}", .0))]
    Transient(String),
    #[error("{}", ictl_l10n::tr!("账号下没有可用的开发团队", "The account has no usable development team"))]
    NoTeam,
    #[error("{}", ictl_l10n::trf!("签名失败：{0}", "Signing failed: {0}", .0))]
    Codesign(ictl_l10n::Text),
    #[error("{}", ictl_l10n::trf!("开发者服务：{0}", "Developer services: {0}", .0))]
    Developer(ictl_l10n::Text),
    #[error("{}", ictl_l10n::trf!("签名结果检查不通过：{0}", "The signed result failed verification: {0}", .0))]
    Verify(ictl_l10n::Text),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl SignError {
    /// Whether this error reflects a transient network, Anisette, or upstream availability problem rather than a
    /// rejected/expired Apple ID session.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Returns true when a [`plume_core::Error`] comes from a transient Anisette, network, or upstream availability
/// failure instead of a real Apple ID session expiration or credential rejection.
fn is_transient_plume_error(e: &plume_core::Error) -> bool {
    match e {
        plume_core::Error::Anisette(_) => true,
        plume_core::Error::Reqwest(re) => !matches!(re.status().map(|s| s.as_u16()), Some(401 | 403)),
        plume_core::Error::GeneralHttpError { status, .. } => matches!(status, 408 | 429 | 500..=599),
        plume_core::Error::DeveloperApi { http_code: Some(code), .. } => matches!(code, 408 | 429 | 500..=599),
        // `qh_send_request` / `v1_send_request` parse the raw response body without checking HTTP status first, so a
        // proxy/gateway 502/503 HTML page surfaces as Plist/SerdeJson rather than Reqwest; real Apple session
        // rejections arrive as valid plists/JSON (`DeveloperApi`).
        plume_core::Error::Io(_) | plume_core::Error::Plist(_) | plume_core::Error::SerdeJson(_) => true,
        _ => false,
    }
}

fn classify_session_error(e: plume_core::Error) -> SignError {
    if is_transient_plume_error(&e) {
        SignError::Transient(e.to_string())
    } else {
        SignError::SessionInvalid(e.to_string())
    }
}

/// Retry `op` up to `delays.len() + 1` times while it returns a transient [`SignError`]. Non-transient errors
/// (such as [`SignError::SessionInvalid`] or [`SignError::NoTeam`]) fail immediately without sleeping.
async fn retry_transient<T, Op, Fut, Sleep, SleepFut>(delays: &[Duration], mut op: Op, mut sleep: Sleep) -> Result<T>
where
    Op: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
    Sleep: FnMut(Duration) -> SleepFut,
    SleepFut: std::future::Future<Output = ()>,
{
    let total = delays.len() + 1;
    for attempt in 1..=total {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_transient() && attempt < total => {
                let delay = delays[attempt - 1];
                tracing::warn!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "Anisette/苹果服务请求临时失败（第 {attempt}/{total} 次），{} 秒后重试：{e}",
                        "Transient Anisette/Apple request failure (attempt {attempt}/{total}), retrying in {} s: {e}",
                        delay.as_secs()
                    )
                );
                sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop always returns on attempt == total")
}

impl From<plume_core::Error> for SignError {
    fn from(e: plume_core::Error) -> Self {
        if is_transient_plume_error(&e) {
            Self::Transient(e.to_string())
        } else {
            Self::Developer(e.to_string().into())
        }
    }
}

impl From<plume_core::AppleCodesignError> for SignError {
    fn from(e: plume_core::AppleCodesignError) -> Self {
        Self::Codesign(e.to_string().into())
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

    async fn fetch_anisette_with<Sleep, SleepFut>(
        &self,
        delays: &[Duration],
        attempt_timeout: Duration,
        sleep: Sleep,
    ) -> Result<plume_core::auth::anisette_data::AnisetteData>
    where
        Sleep: FnMut(Duration) -> SleepFut,
        SleepFut: std::future::Future<Output = ()>,
    {
        retry_transient(
            delays,
            |_attempt| async {
                let cfg = self.anisette();
                match tokio::time::timeout(attempt_timeout, plume_core::auth::anisette_data::AnisetteData::new(cfg))
                    .await
                {
                    Ok(Ok(data)) => Ok(data),
                    Ok(Err(e)) => Err(SignError::from(e)),
                    Err(_) => Err(SignError::Transient(format!(
                        "Anisette request timed out after {} s",
                        attempt_timeout.as_secs()
                    ))),
                }
            },
            sleep,
        )
        .await
    }

    /// 交互式登录。`two_factor` 在需要验证码时被调用（可在里面读终端输入）。成功后会话写入 `accounts.json`。
    pub async fn login(
        &self,
        email: &str,
        password: &str,
        two_factor: impl Fn(TwoFactorRequest) -> std::result::Result<TwoFactorAction, String>,
    ) -> Result<String> {
        restrict_dir(&self.dir)?;
        let anisette =
            self.fetch_anisette_with(&SESSION_RETRY_DELAYS, SESSION_ATTEMPT_TIMEOUT, tokio::time::sleep).await?;
        let (e, p) = (email.to_owned(), password.to_owned());
        let account =
            PlumeAccount::login_with_anisette(move || Ok((e.clone(), p.clone())), two_factor, anisette).await?;
        let mut store = AccountStore::load(&Some(self.store_path()))
            .await
            .map_err(|e| SignError::Developer(e.to_string().into()))?;
        store
            .accounts_add_from_session(email.to_owned(), account)
            .await
            .map_err(|e| SignError::Developer(e.to_string().into()))?;
        // the session's tokens were just written with the default umask
        restrict_tree(&self.dir)?;
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
        let store = AccountStore::load(&Some(self.store_path()))
            .await
            .map_err(|e| SignError::Developer(e.to_string().into()))?;
        store.selected_account().map(|a| a.email().clone()).ok_or(SignError::NotLoggedIn)
    }

    /// 用保存的令牌恢复会话，取第一个团队；对 Anisette / 苹果服务的瞬断做有限次退避重试，避免单次抖动误判会话失效
    pub async fn session(&self) -> Result<Session> {
        restrict_dir(&self.dir)?;
        restrict_tree(&self.dir)?;
        let store = AccountStore::load(&Some(self.store_path()))
            .await
            .map_err(|e| SignError::Developer(e.to_string().into()))?;
        let acct = store.selected_account().ok_or(SignError::NotLoggedIn)?.clone();
        let adsid = acct.adsid().clone();
        let gs_token = acct.xcode_gs_token().clone();
        retry_transient(
            &SESSION_RETRY_DELAYS,
            |_attempt| async {
                match tokio::time::timeout(SESSION_ATTEMPT_TIMEOUT, self.connect_session(&adsid, &gs_token)).await {
                    Ok(res) => res,
                    Err(_) => Err(SignError::Transient(format!(
                        "session request timed out after {} s",
                        SESSION_ATTEMPT_TIMEOUT.as_secs()
                    ))),
                }
            },
            tokio::time::sleep,
        )
        .await
    }

    async fn connect_session(&self, adsid: &str, gs_token: &str) -> Result<Session> {
        let inner = DeveloperSession::new(adsid.to_owned(), gs_token.to_owned(), self.anisette())
            .await
            .map_err(classify_session_error)?;
        let teams = inner.qh_list_teams().await.map_err(classify_session_error)?.teams;
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
        // removed however this ends, also when the caller gives up waiting and drops it
        let work = WorkDir(std::env::temp_dir().join(format!("ictl-sign-{}", uuid::Uuid::new_v4())));
        let result = self.sign_in(&work.0, input, output, udid).await;
        // a key written on the way is locked down even when signing failed after it
        let _ = restrict_tree(&self.dir.dir);
        result
    }

    async fn sign_in(&self, work: &Path, input: &Path, output: &Path, udid: &str) -> Result<Signed> {
        let app = ipa::unpack(input, work)?;
        let bundles = bundle::collect_sorted(&app)?;
        let main =
            bundles.last().cloned().ok_or_else(|| SignError::Codesign(ictl_l10n::text!("空的 .app", "empty .app")))?;
        let orig = main.identifier().ok_or_else(|| {
            SignError::Codesign(ictl_l10n::text!(
                "主程序没有 CFBundleIdentifier",
                "the main executable has no CFBundleIdentifier"
            ))
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
        // a new certificate's private key was just written with the default umask (0644): locked down now, not after
        // the whole package is signed
        restrict_tree(&self.dir.dir)?;
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
                SignError::Developer(ictl_l10n::textf!(
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
            return Err(SignError::Verify(ictl_l10n::textf!("{} 没有签名", "{} is not signed", b.dir.display())));
        }
    }
    let p = Profile::load(&app_dir.join("embedded.mobileprovision")).ok_or_else(|| {
        SignError::Verify(ictl_l10n::text!(
            "主程序没有可解析的 embedded.mobileprovision",
            "the main executable has no readable embedded.mobileprovision"
        ))
    })?;
    if !p.devices.iter().any(|d| d == udid) {
        return Err(SignError::Verify(ictl_l10n::text!(
            "描述文件里没有这台手机，先注册设备",
            "this phone is not in the provisioning profile; register the device first"
        )));
    }
    if !p.get_task_allow {
        return Err(SignError::Verify(ictl_l10n::text!(
            "描述文件不带 get-task-allow，WDA 起不来",
            "the provisioning profile lacks get-task-allow, WDA won't start"
        )));
    }
    if let Some(der) = signing_cert
        && !p.certificates.iter().any(|c| c == der)
    {
        return Err(SignError::Verify(ictl_l10n::text!(
            "描述文件里没有签名用的证书（证书在描述文件生成后才换过），手机会拒装",
            "the provisioning profile doesn't contain the signing certificate (the certificate changed after the profile was created); the phone would refuse to install it"
        )));
    }
    Ok(p)
}

/// [`Session::sign_ipa`]'s scratch directory, removed when dropped
struct WorkDir(PathBuf);

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

    /// 取 anisette 头只有名字不分大小写，值要原样发：`X-MMe-Client-Info` 里的 `Mac OS X` 变成小写，
    /// 苹果 GSA 直接回 403，登录失败（#171）。plume_core 在 vendor 里、不在 workspace，测试放在这里才会在 CI 跑
    #[test]
    fn anisette_header_values_keep_their_case() {
        use plume_core::auth::anisette_data::AnisetteData;
        let anisette = AnisetteData {
            base_headers: std::collections::HashMap::from([
                (
                    "X-Mme-Client-Info".to_owned(),
                    "<MacBookPro13,3> <Mac OS X;10.12.6;16G29> <com.apple.AuthKit/1 (com.apple.dt.Xcode/3594.4.19)>"
                        .to_owned(),
                ),
                ("X-Apple-Locale".to_owned(), "en_US".to_owned()),
            ]),
            generated_at: SystemTime::now(),
            config: AnisetteConfiguration::default(),
        };
        assert_eq!(
            anisette.get_header("x-mme-client-info").unwrap(),
            "<MacBookPro13,3> <Mac OS X;10.12.6;16G29> <com.apple.AuthKit/1 (com.apple.akd/1.0)>"
        );
        assert_eq!(anisette.get_header("x-apple-locale").unwrap(), "en_US");
        assert_eq!(anisette.get_header("X-Apple-Locale").unwrap(), "en_US");
    }
    use std::time::SystemTime;

    /// The scratch directory goes with the guard, also when the signing future is dropped (a caller's timeout)
    #[test]
    fn the_scratch_directory_is_removed_when_dropped() {
        let dir = std::env::temp_dir().join(format!("ictl-sign-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("Payload/X.app")).unwrap();
        drop(WorkDir(dir.clone()));
        assert!(!dir.exists());
    }

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
        let cert_err =
            ictl_l10n::with_lang(ictl_l10n::Lang::Zh, || verify_signed(&app, "UDID-1", Some(b"CERT-B")).unwrap_err());
        assert!(ictl_l10n::with_lang(ictl_l10n::Lang::Zh, || cert_err.to_string()).contains("签名用的证书"));
        let en = ictl_l10n::with_lang(ictl_l10n::Lang::En, || cert_err.to_string());
        assert!(en.contains("The signed result failed verification:") && en.contains("signing certificate"), "{en}");
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

    #[test]
    fn plume_error_classification_separates_transient_from_session_invalid() {
        let transient_cases = vec![
            plume_core::Error::GeneralHttpError { url: "https://example.com".into(), status: 408 },
            plume_core::Error::GeneralHttpError { url: "https://example.com".into(), status: 429 },
            plume_core::Error::GeneralHttpError { url: "https://example.com".into(), status: 502 },
            plume_core::Error::GeneralHttpError { url: "https://example.com".into(), status: 503 },
            plume_core::Error::DeveloperApi {
                url: "https://developerservices2.apple.com".into(),
                result_code: 500,
                http_code: Some(503),
                message: "Service Unavailable".into(),
            },
            plume_core::Error::DeveloperApi {
                url: "https://developerservices2.apple.com".into(),
                result_code: 429,
                http_code: Some(429),
                message: "Too Many Requests".into(),
            },
            plume_core::Error::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")),
            plume_core::Error::Plist(
                plist::from_bytes::<plist::Dictionary>(b"<html>502 Bad Gateway</html>").unwrap_err(),
            ),
            plume_core::Error::SerdeJson(serde_json::from_str::<serde_json::Value>("<html>503</html>").unwrap_err()),
        ];
        for err in transient_cases {
            assert!(is_transient_plume_error(&err), "expected transient: {err}");
            let classified = classify_session_error(err);
            assert!(matches!(classified, SignError::Transient(_)), "expected Transient, got {classified:?}");
            assert!(classified.is_transient());
        }

        let invalid_cases = vec![
            plume_core::Error::GeneralHttpError { url: "https://example.com".into(), status: 401 },
            plume_core::Error::GeneralHttpError { url: "https://example.com".into(), status: 403 },
            plume_core::Error::DeveloperApi {
                url: "https://developerservices2.apple.com".into(),
                result_code: 1100,
                http_code: Some(200),
                message: "Your session has expired. Please log in.".into(),
            },
            plume_core::Error::DeveloperApi {
                url: "https://developerservices2.apple.com".into(),
                result_code: 22411,
                http_code: None,
                message: "Unauthorized".into(),
            },
            plume_core::Error::DeveloperSessionRequestFailed,
            plume_core::Error::AuthSrpWithMessage(-20101, "Invalid credentials".into()),
            plume_core::Error::Bad2faCode,
        ];
        for err in invalid_cases {
            assert!(!is_transient_plume_error(&err), "expected non-transient: {err}");
            let classified = classify_session_error(err);
            assert!(matches!(classified, SignError::SessionInvalid(_)), "expected SessionInvalid, got {classified:?}");
            assert!(!classified.is_transient());
        }

        let dev_err = SignError::from(plume_core::Error::DeveloperApi {
            url: "https://developerservices2.apple.com".into(),
            result_code: 9401,
            http_code: Some(200),
            message: "App ID limit reached".into(),
        });
        assert!(matches!(dev_err, SignError::Developer(_)));
        let transient_from =
            SignError::from(plume_core::Error::GeneralHttpError { url: "https://example.com".into(), status: 502 });
        assert!(matches!(transient_from, SignError::Transient(_)));
        ictl_l10n::with_lang(ictl_l10n::Lang::Zh, || {
            assert!(transient_from.to_string().contains("暂时连不上 Anisette 或苹果服务"));
        });
        ictl_l10n::with_lang(ictl_l10n::Lang::En, || {
            assert!(transient_from.to_string().contains("Temporarily unable to reach the Anisette or Apple service"));
        });
    }

    #[tokio::test]
    async fn retry_transient_backs_off_recovers_and_skips_non_transient_errors() {
        use std::cell::RefCell;

        // Recovers on attempt 3 after two transient failures.
        let slept = RefCell::new(Vec::new());
        let attempts = RefCell::new(0usize);
        let ok = retry_transient(
            &SESSION_RETRY_DELAYS,
            |n| {
                *attempts.borrow_mut() += 1;
                async move { if n < 3 { Err(SignError::Transient(format!("glitch {n}"))) } else { Ok("recovered") } }
            },
            |d| {
                slept.borrow_mut().push(d);
                async {}
            },
        )
        .await
        .unwrap();
        assert_eq!(ok, "recovered");
        assert_eq!(*attempts.borrow(), 3);
        assert_eq!(&*slept.borrow(), &SESSION_RETRY_DELAYS);

        // Exhausts all 3 attempts on persistent transient failures.
        slept.borrow_mut().clear();
        *attempts.borrow_mut() = 0;
        let err = retry_transient::<(), _, _, _, _>(
            &SESSION_RETRY_DELAYS,
            |n| {
                *attempts.borrow_mut() += 1;
                async move { Err(SignError::Transient(format!("down {n}"))) }
            },
            |d| {
                slept.borrow_mut().push(d);
                async {}
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SignError::Transient(ref m) if m == "down 3"), "{err:?}");
        assert_eq!(*attempts.borrow(), 3);
        assert_eq!(&*slept.borrow(), &SESSION_RETRY_DELAYS);

        // Non-transient errors fail immediately without sleeping or retrying.
        for make_err in
            [|| SignError::SessionInvalid("expired".into()), || SignError::NoTeam, || SignError::NotLoggedIn]
        {
            slept.borrow_mut().clear();
            *attempts.borrow_mut() = 0;
            let err = retry_transient::<(), _, _, _, _>(
                &SESSION_RETRY_DELAYS,
                |_n| {
                    *attempts.borrow_mut() += 1;
                    let e = make_err();
                    async move { Err(e) }
                },
                |d| {
                    slept.borrow_mut().push(d);
                    async {}
                },
            )
            .await
            .unwrap_err();
            assert!(!err.is_transient(), "{err:?}");
            assert_eq!(*attempts.borrow(), 1);
            assert!(slept.borrow().is_empty());
        }
    }

    /// Write a pre-provisioned SideStore Anisette v3 `state.plist` so local mock tests exercise `/v3/client_info`
    /// and `/v3/get_headers` without touching Apple's live GSA servers.
    fn write_provisioned_anisette_state(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("keychain_identifier".into(), plist::Value::Data(vec![0x11; 16]));
        dict.insert("adi_pb".into(), plist::Value::Data(vec![0x22; 32]));
        plist::to_file_xml(dir.join("state.plist"), &plist::Value::Dictionary(dict)).unwrap();
    }

    async fn read_http_request_head(stream: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let header_end = loop {
            let n = stream.read(&mut tmp).await.unwrap();
            if n == 0 {
                break buf.len();
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if buf.len() > 8192 {
                break buf.len();
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let content_len = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok()).flatten()
            })
            .unwrap_or(0);
        while buf.len() < header_end.saturating_add(content_len) {
            let n = stream.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        head
    }

    async fn write_http_response(stream: &mut tokio::net::TcpStream, status: &str, content_type: &str, body: &str) {
        use tokio::io::AsyncWriteExt;
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
    }

    #[tokio::test]
    async fn anisette_v3_transient_client_info_glitch_recovers_via_retry() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join(format!("ictl-ani-recover-{}", uuid::Uuid::new_v4()));
        write_provisioned_anisette_state(&dir);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut client_info_hits = 0usize;
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req = read_http_request_head(&mut stream).await;
                if req.starts_with("GET /v3/client_info ") {
                    client_info_hits += 1;
                    if client_info_hits == 1 {
                        write_http_response(&mut stream, "502 Bad Gateway", "text/html", "<html>502</html>").await;
                    } else {
                        let body = r#"{"client_info":"<MacBookPro13,3> <Mac OS X;10.12.6;16G29> <com.apple.AuthKit/1 (com.apple.dt.Xcode/3594.4.19)>","user_agent":"akd/1.0"}"#;
                        write_http_response(&mut stream, "200 OK", "application/json", body).await;
                    }
                } else if req.starts_with("POST /v3/get_headers ") {
                    let body = r#"{"result":"Headers","X-Apple-I-MD-M":"MID","X-Apple-I-MD":"OTP","X-Apple-I-MD-RINFO":"17106176"}"#;
                    write_http_response(&mut stream, "200 OK", "application/json", body).await;
                    break;
                }
            }
        });

        // Trailing slash verifies URL normalization in omnisette as well.
        let acct = AccountDir { dir: dir.clone(), anisette_url: Some(format!("http://{addr}/")) };
        let slept = std::cell::RefCell::new(Vec::new());
        let data = acct
            .fetch_anisette_with(&SESSION_RETRY_DELAYS, Duration::from_secs(5), |d| {
                slept.borrow_mut().push(d);
                async {}
            })
            .await
            .unwrap();
        assert_eq!(data.get_header("X-Apple-I-MD").unwrap(), "OTP");
        assert_eq!(data.get_header("X-Apple-I-MD-M").unwrap(), "MID");
        assert_eq!(&*slept.borrow(), &[SESSION_RETRY_DELAYS[0]]);

        server.await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn anisette_v3_get_headers_errors_and_timeouts_return_transient_without_panicking() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Scenario A & B: /v3/get_headers returns 503 HTML, then a non--45061 GetHeadersError JSON.
        // Upstream omnisette panicked on both (remote_anisette_v3.rs L266 & L521); verify both become SignError::Transient.
        for (status, ctype, body, expected_substr) in [
            ("503 Service Unavailable", "text/html", "<html>overloaded</html>", "503"),
            (
                "200 OK",
                "application/json",
                r#"{"result":"GetHeadersError","message":"anisette backend busy"}"#,
                "anisette backend busy",
            ),
        ] {
            let dir = std::env::temp_dir().join(format!("ictl-ani-err-{}", uuid::Uuid::new_v4()));
            write_provisioned_anisette_state(&dir);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                // 3 attempts * (GET client_info + POST get_headers)
                for _ in 0..6 {
                    let Ok((mut stream, _)) = listener.accept().await else { break };
                    let req = read_http_request_head(&mut stream).await;
                    if req.starts_with("GET /v3/client_info ") {
                        let ok = r#"{"client_info":"<MacBookPro13,3> <Mac OS X;10.12.6;16G29> <com.apple.AuthKit/1 (com.apple.dt.Xcode/3594.4.19)>","user_agent":"akd/1.0"}"#;
                        write_http_response(&mut stream, "200 OK", "application/json", ok).await;
                    } else if req.starts_with("POST /v3/get_headers ") {
                        write_http_response(&mut stream, status, ctype, body).await;
                    }
                }
            });

            let acct = AccountDir { dir: dir.clone(), anisette_url: Some(format!("http://{addr}")) };
            let slept = std::cell::RefCell::new(Vec::new());
            let err = acct
                .fetch_anisette_with(&SESSION_RETRY_DELAYS, Duration::from_secs(5), |d| {
                    slept.borrow_mut().push(d);
                    async {}
                })
                .await
                .unwrap_err();
            assert!(matches!(err, SignError::Transient(ref msg) if msg.contains(expected_substr)), "{err:?}");
            assert_eq!(&*slept.borrow(), &SESSION_RETRY_DELAYS);
            server.await.unwrap();
            std::fs::remove_dir_all(dir).unwrap();
        }

        // Scenario C: stalled server never replies; per-attempt timeout cuts it off and retries cleanly.
        let dir = std::env::temp_dir().join(format!("ictl-ani-hang-{}", uuid::Uuid::new_v4()));
        write_provisioned_anisette_state(&dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            for _ in 0..3 {
                let Ok((stream, _)) = listener.accept().await else { break };
                held.push(stream);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let acct = AccountDir { dir: dir.clone(), anisette_url: Some(format!("http://{addr}")) };
        let slept = std::cell::RefCell::new(Vec::new());
        let err = acct
            .fetch_anisette_with(&SESSION_RETRY_DELAYS, Duration::from_millis(120), |d| {
                slept.borrow_mut().push(d);
                async {}
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SignError::Transient(ref msg) if msg.contains("timed out")), "{err:?}");
        assert_eq!(&*slept.borrow(), &SESSION_RETRY_DELAYS);
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
