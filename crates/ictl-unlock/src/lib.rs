//! 锁屏自动解锁（默认关闭，需显式开启并配置密码文件）。
//!
//! 必须遵守的规则（toolkit 实测，见 docs/capability-matrix.md §5）：
//! 1. 唤醒固定为：Home → 等 1.0s → 底部上滑 → 等 1.5s；不以"树里有键盘"作判断
//! 2. 先读密码栏「共N位数，已输入M位」，总位数不符一位都不输
//! 3. 每位一个独立 W3C 手势（按下 80ms），按完核对已输入位数 +1，不符立即停
//! 4. 期间 waitForIdleTimeout=1、animationCoolOffTimeout=0.2，结束后恢复（0 会唤醒失败）
//! 5. 第一位没生效可重来（最多 3 轮）；整串输完仍锁着才算输错：写停用标记 + 通知
//! 6. 只支持纯数字；开机后首次解锁（BFU）永远需要人工

use std::path::PathBuf;

pub const WAKE_SETTLE_MS: u64 = 1000;
pub const SWIPE_SETTLE_MS: u64 = 1500;
pub const KEY_PRESS_MS: u64 = 80;
pub const FIRST_DIGIT_ROUNDS: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    #[error("没有配置锁屏密码，需要人工解锁")]
    NoPasscode,
    #[error("自动解锁之前失败过已停用，请人工解锁并检查配置后清除停用标记")]
    Disabled,
    #[error("调不出密码键盘（一位都没输）")]
    NoKeypad,
    /// 输入不完整，不算输错
    #[error("第 {0} 位没按上，已停止（输入不完整不算输错）")]
    Incomplete(u32),
    #[error("手机要求 {expected} 位密码，配置的是 {configured} 位，一位都没输")]
    LengthMismatch { expected: u32, configured: u32 },
    /// 整串输完仍锁着：已写停用标记
    #[error("整串密码输完手机仍锁着，自动解锁已停用")]
    WrongPasscode,
    #[error(transparent)]
    Wda(#[from] ictl_wda::WdaError),
}

pub struct UnlockConfig {
    pub passcode_file: PathBuf,
    pub disabled_flag: PathBuf,
}

/// 未锁屏直接返回 Ok；否则按规则解锁
pub async fn ensure_unlocked(wda: &mut ictl_wda::WdaClient, cfg: &UnlockConfig) -> Result<(), UnlockError> {
    let _ = (wda, cfg);
    todo!()
}
