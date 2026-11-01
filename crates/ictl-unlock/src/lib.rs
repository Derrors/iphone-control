//! 锁屏自动解锁（默认关闭，需显式配置密码文件）。
//!
//! 规则全部来自 toolkit 实测（docs/capability-matrix.md §5）：
//! 1. 先读 `/wda/locked`，没锁直接返回；WDA 不可用也直接返回（交给后面的调用去报错）
//! 2. 每轮固定：按 Home → 等 1.0s → 底部上滑 → 等 1.5s。不以「树里有键盘」判断：键盘收起时按键元素也常驻树中
//! 3. 读密码栏「共N位数，已输入M位」：总位数和配置不符一位都不输；有残留输入就下一轮（Home 会清空）
//! 4. 每位一个独立 W3C 手势（按下 80ms），按完核对已输入位数正好 +1，不符立即停——输入不完整不算输错。
//!    多位合成一个手势会漏键；`/wda/tap` 在锁屏上每次约 2.9 秒，太慢
//! 5. 解锁期间 waitForIdleTimeout=1、animationCoolOffTimeout=0.2（0 会唤醒失败），结束后恢复
//! 6. 第一位没生效可以重来（最多 3 轮）；整串输完仍锁着才算真正输错一次：写停用标记，之后拒绝自动解锁直到人工清除
//! 7. 只支持纯数字；开机后首次解锁（BFU）永远需要人工

use std::path::PathBuf;
use std::time::Duration;

use ictl_wda::{IdleSettings, WdaClient, WdaError, elements, http::Transport, swipe_steps, tap_steps};

#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    #[error("手机已锁屏，且没有配置锁屏密码，需要人工解锁")]
    NoPasscode,
    #[error("自动解锁只支持纯数字锁屏密码")]
    NotDigits,
    #[error("手机已锁屏；自动解锁之前失败过已停用，请人工解锁、核对密码后运行 iphone-control unlock-reset")]
    Disabled,
    #[error("调不出锁屏密码键盘（一位都没输进去），请截图看看锁屏界面")]
    NoKeypad,
    /// 输入不完整，不算输错
    #[error("第 {0} 位没按上，已停止（输入不完整不算输错），稍后再试")]
    Incomplete(u32),
    #[error("手机要求 {expected} 位密码，配置的是 {configured} 位，一位都没输")]
    LengthMismatch { expected: u32, configured: u32 },
    /// 整串输完仍锁着：已写停用标记
    #[error("整串密码输完手机仍锁着，自动解锁已停用（防止输错次数累计）")]
    WrongPasscode,
    #[error(transparent)]
    Wda(#[from] WdaError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, UnlockError>;

/// 各步等待时长。默认值是实测可靠的；单测里设为 0
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub wake_settle: Duration,
    pub swipe_settle: Duration,
    pub key_press_ms: u64,
    pub key_confirm: Duration,
    pub unlock_confirm: Duration,
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            wake_settle: Duration::from_millis(1000),
            swipe_settle: Duration::from_millis(1500),
            key_press_ms: 80,
            key_confirm: Duration::from_millis(1500),
            unlock_confirm: Duration::from_secs(5),
            poll: Duration::from_millis(300),
        }
    }
}

pub const FIRST_DIGIT_ROUNDS: u32 = 3;
/// 解锁期间的等待上限（不能是 0：唤醒会失败）
pub const UNLOCK_IDLE: IdleSettings = IdleSettings { wait_for_idle_timeout: 1.0, animation_cool_off_timeout: 0.2 };

pub struct UnlockConfig {
    pub passcode_file: PathBuf,
    pub disabled_flag: PathBuf,
    pub timing: Timing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 本来就没锁，或 WDA 不可用（不处理）
    NotLocked,
    Unlocked,
}

impl UnlockConfig {
    pub fn enabled(&self) -> bool {
        self.passcode_file.is_file()
    }

    pub fn disabled_by_failure(&self) -> bool {
        self.disabled_flag.exists()
    }
}

/// 未锁屏直接返回；否则按规则解锁
pub async fn ensure_unlocked<T: Transport>(wda: &mut WdaClient<T>, cfg: &UnlockConfig) -> Result<Outcome> {
    // 先只问 WDA：确定没锁就直接返回，日常操作不多发请求
    let reported = match wda.wda_reports_locked().await {
        Ok(v) => v,
        Err(WdaError::Unavailable(_)) => return Ok(Outcome::NotLocked),
        Err(e) => return Err(e.into()),
    };
    if !reported && wda.lock_state_trusted() {
        return Ok(Outcome::NotLocked);
    }
    let saved = wda.idle_settings().await?;
    wda.set_idle_settings(UNLOCK_IDLE).await?;
    let r = check_and_unlock(wda, cfg, reported).await;
    if let Err(e) = wda.set_idle_settings(saved).await {
        tracing::warn!("恢复 WDA 等待设置失败：{e}");
    }
    r
}

/// 可用的密码；不能自动解锁时给出原因（停用、没配、不是纯数字）
fn passcode(cfg: &UnlockConfig) -> Result<String> {
    if cfg.disabled_by_failure() {
        return Err(UnlockError::Disabled);
    }
    let code = std::fs::read_to_string(&cfg.passcode_file).map_err(|_| UnlockError::NoPasscode)?.trim().to_owned();
    if code.is_empty() || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(UnlockError::NotDigits);
    }
    Ok(code)
}

/// `reported`：WDA 自己说锁了。熄屏锁屏时第一个无障碍查询（取元素树、前台 App）要等约 8 秒，
/// 所以能自动解锁时不先核对，直接进解锁流程——第一步按 Home 唤醒屏幕；若其实是通知中心（WDA 误报），
/// Home 会收起它，随后核对为未锁屏，一位都不输。不能自动解锁时才先核对，免得把通知中心报成「需要人工解锁」
async fn check_and_unlock<T: Transport>(wda: &mut WdaClient<T>, cfg: &UnlockConfig, reported: bool) -> Result<Outcome> {
    let code = passcode(cfg);
    if !(reported && code.is_ok()) {
        match wda.locked().await {
            Ok(false) | Err(WdaError::Unavailable(_)) => return Ok(Outcome::NotLocked),
            Ok(true) => {}
            Err(e) => return Err(e.into()),
        }
    }
    unlock(wda, cfg, code?.as_bytes()).await
}

async fn press<T: Transport>(wda: &mut WdaClient<T>, at: (i64, i64), t: &Timing) -> Result<()> {
    Ok(wda.actions(&tap_steps(at.0 as f64, at.1 as f64, t.key_press_ms)).await?)
}

/// 按一个数字键，等密码栏已输入位数变成 `expect`；返回实际位数（读不到密码栏为 -1）
async fn tap_key<T: Transport>(wda: &mut WdaClient<T>, at: (i64, i64), expect: u32, t: &Timing) -> Result<i64> {
    press(wda, at, t).await?;
    let deadline = tokio::time::Instant::now() + t.key_confirm;
    loop {
        let got = wda.passcode_field().await?.map(|f| f.1 as i64).unwrap_or(-1);
        if got >= expect as i64 || tokio::time::Instant::now() >= deadline {
            return Ok(got);
        }
        tokio::time::sleep(t.poll.min(Duration::from_millis(150))).await;
    }
}

async fn unlock<T: Transport>(wda: &mut WdaClient<T>, cfg: &UnlockConfig, code: &[u8]) -> Result<Outcome> {
    let t = &cfg.timing;
    let screen = wda.screen_size().await?;
    let digit = |i: usize| (code[i] - b'0') as usize;
    let len = code.len() as u32;

    let mut keys = None;
    for _ in 0..FIRST_DIGIT_ROUNDS {
        // Home 唤醒；键盘已打开时会收起并清空半截输入
        // 熄屏锁屏刚唤醒时上滑偶尔卡十几秒后报 `point.x != INFINITY`（WDA 还拿不到界面尺寸）：一位没输，算这轮落空重来
        let woke = async {
            wda.press_button("home").await?;
            tokio::time::sleep(t.wake_settle).await;
            let (cx, h) = (screen.width / 2.0, screen.height);
            wda.actions(&swipe_steps((cx, h - 8.0), (cx, h * 0.45), 250)).await
        }
        .await;
        if let Err(e) = woke {
            match e {
                WdaError::Remote { .. } => {
                    tracing::info!("唤醒/上滑失败，重来一轮：{e}");
                    continue;
                }
                other => return Err(other.into()),
            }
        }
        tokio::time::sleep(t.swipe_settle).await;
        if !wda.locked().await? {
            return Ok(Outcome::Unlocked);
        }
        let (Some(pad), Some((total, entered))) =
            (elements::keypad(&wda.elements().await?), wda.passcode_field().await?)
        else {
            continue;
        };
        if entered != 0 {
            continue;
        }
        if total != len {
            return Err(UnlockError::LengthMismatch { expected: total, configured: len });
        }
        match tap_key(wda, pad[digit(0)], 1, t).await? {
            1 => {
                keys = Some(pad);
                break;
            }
            n if n > 1 => return Err(UnlockError::Incomplete(1)),
            _ => continue,
        }
    }
    let pad = keys.ok_or(UnlockError::NoKeypad)?;

    for n in 2..len {
        if tap_key(wda, pad[digit(n as usize - 1)], n, t).await? != n as i64 {
            return Err(UnlockError::Incomplete(n));
        }
    }

    press(wda, pad[digit(code.len() - 1)], t).await?;
    let deadline = tokio::time::Instant::now() + t.unlock_confirm;
    loop {
        tokio::time::sleep(t.poll).await;
        if !wda.locked().await? {
            return Ok(Outcome::Unlocked);
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    std::fs::write(&cfg.disabled_flag, format!("{now} 整串密码输完仍是锁屏状态\n"))?;
    Err(UnlockError::WrongPasscode)
}

#[cfg(test)]
mod fake;

#[cfg(test)]
mod tests {
    use super::*;
    use fake::FakeLockScreen;

    fn cfg(passcode: Option<&str>) -> (UnlockConfig, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ictl-unlock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(p) = passcode {
            std::fs::write(dir.join("passcode"), p).unwrap();
        }
        let timing = Timing {
            wake_settle: Duration::ZERO,
            swipe_settle: Duration::ZERO,
            key_press_ms: 80,
            key_confirm: Duration::from_millis(20),
            unlock_confirm: Duration::from_millis(20),
            poll: Duration::from_millis(1),
        };
        (UnlockConfig { passcode_file: dir.join("passcode"), disabled_flag: dir.join("UNLOCK-DISABLED"), timing }, dir)
    }

    #[tokio::test]
    async fn correct_passcode_unlocks_and_restores_idle_settings() {
        let fake = FakeLockScreen::new("123456");
        let (cfg, dir) = cfg(Some("123456\n"));
        let mut wda = WdaClient::new(&fake);
        assert_eq!(ensure_unlocked(&mut wda, &cfg).await.unwrap(), Outcome::Unlocked);
        let s = fake.state();
        assert!(!s.locked);
        assert_eq!(s.idle, (10.0, 2.0), "结束后要恢复原来的等待设置");
        assert!(s.idle_during_unlock.iter().all(|i| *i == (1.0, 0.2)));
        assert_eq!(s.presses_per_action_max, 1, "每位一个独立手势");
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn not_locked_does_nothing() {
        let fake = FakeLockScreen::new("123456");
        fake.unlock_now();
        let (cfg, dir) = cfg(None);
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::NotLocked);
        assert_eq!(fake.state().requests_to_actions, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn wrong_passcode_counts_once_and_disables() {
        let fake = FakeLockScreen::new("123456");
        let (cfg, dir) = cfg(Some("654321"));
        let mut wda = WdaClient::new(&fake);
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::WrongPasscode)));
        assert_eq!(fake.state().failed_attempts, 1);
        assert!(cfg.disabled_flag.exists());
        // 停用后不再尝试
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::Disabled)));
        assert_eq!(fake.state().failed_attempts, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn length_mismatch_types_nothing() {
        let fake = FakeLockScreen::new("123456");
        let (cfg, dir) = cfg(Some("1234"));
        let r = ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await;
        assert!(matches!(r, Err(UnlockError::LengthMismatch { expected: 6, configured: 4 })), "{r:?}");
        assert_eq!(fake.state().digits_typed, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 回归用例 #1：键盘收起时按键仍在树里。第一次上滑没调出键盘（按键落空），不算输错，下一轮重来
    #[tokio::test]
    async fn keypad_in_tree_but_not_shown_retries_without_counting() {
        let fake = FakeLockScreen::new("123456");
        fake.swipes_ignored(1);
        let (cfg, dir) = cfg(Some("123456"));
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::Unlocked);
        assert_eq!(fake.state().failed_attempts, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn swipe_error_right_after_wake_retries_round() {
        let fake = FakeLockScreen::new("123456");
        fake.swipe_errors(1);
        let (cfg, dir) = cfg(Some("123456"));
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::Unlocked);
        assert_eq!(fake.state().failed_attempts, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn keypad_never_shows_is_no_keypad_not_wrong() {
        let fake = FakeLockScreen::new("123456");
        fake.swipes_ignored(99);
        let (cfg, dir) = cfg(Some("123456"));
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await, Err(UnlockError::NoKeypad)));
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 回归用例 #4：中间某位被吞（键盘没接住），立即停止，不提交、不算输错
    #[tokio::test]
    async fn dropped_middle_digit_stops_without_submitting() {
        let fake = FakeLockScreen::new("123456");
        fake.drop_press(3);
        let (cfg, dir) = cfg(Some("123456"));
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await, Err(UnlockError::Incomplete(3))));
        assert_eq!(fake.state().failed_attempts, 0);
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn missing_or_non_digit_passcode() {
        let fake = FakeLockScreen::new("123456");
        let (cfg_none, d1) = cfg(None);
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg_none).await, Err(UnlockError::NoPasscode)));
        let (cfg_alpha, d2) = cfg(Some("abc123"));
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg_alpha).await, Err(UnlockError::NotDigits)));
        std::fs::remove_dir_all(d1).unwrap();
        std::fs::remove_dir_all(d2).unwrap();
    }
}
