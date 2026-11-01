//! 锁屏自动解锁（默认关闭，需显式配置密码文件）。
//!
//! 规则全部来自 toolkit 实测（docs/capability-matrix.md §5）：
//! 1. 先读 `/wda/locked`，没锁直接返回；WDA 不可用也直接返回（交给后面的调用去报错）
//! 2. 每轮固定：按 Home → 等 1.0s → 底部上滑 → 等 1.5s。不以「树里有键盘」判断：键盘收起时按键元素也常驻树中
//! 3. 读密码栏「共N位数，已输入M位」：总位数和配置不符一位都不输；有残留输入先用右下角删除键逐位删到 0
//!    （iOS 27 上 Home 不会清空残留），删不掉就下一轮
//! 4. 每位一个独立 W3C 手势（按下 80ms），按完核对已输入位数正好 +1，不符立即停——确定没凑满整串的不算输错；
//!    多记一下凑满、密码栏归零或消失，按「可能已提交」去确认。多位合成一个手势会漏键；`/wda/tap` 在锁屏上每次约 2.9 秒，太慢
//! 5. 解锁期间 waitForIdleTimeout=1、animationCoolOffTimeout=0.2（0 会唤醒失败），结束后恢复
//! 6. 第一位没生效可以重来（最多 3 轮）。从第 2 位起先写停用标记，确认解开或确认没凑满才撤掉：整串输完仍锁着、
//!    中途出错、进程被杀都保持停用，直到人工清除。确认结果时核对出错一律当仍锁着
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
    /// 第 2 位之后出错，不知道密码是否已提交：停用标记留着
    #[error("输入密码途中出错（{0}），无法确认是否已提交，自动解锁已停用（防止输错次数累计）")]
    Uncertain(String),
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
/// WDA 的默认等待设置（上次没恢复时用它恢复）
pub const WDA_DEFAULT_IDLE: IdleSettings =
    IdleSettings { wait_for_idle_timeout: 10.0, animation_cool_off_timeout: 2.0 };

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
    let code = passcode(cfg);
    // WDA 说锁了且能自动解锁：不先核对（熄屏锁屏时第一个无障碍查询要等约 8 秒），直接进解锁流程——第一步按 Home
    // 唤醒屏幕；若其实是通知中心（WDA 误报），Home 会收起它，随后核对为未锁屏，一位都不输。
    // 其余情况先核对，不动等待设置：不能自动解锁时免得把通知中心报成「需要人工解锁」，WDA 说没锁时确认一次就可信了
    if !(reported && code.is_ok()) {
        match wda.check_lock(Some(reported), false).await {
            Ok(c) if !c.locked => return Ok(Outcome::NotLocked),
            Ok(_) => {}
            Err(WdaError::Unavailable(_)) => return Ok(Outcome::NotLocked),
            Err(e) => return Err(e.into()),
        }
    }
    let code = code?;
    let saved = wda.idle_settings().await?;
    // 上次异常退出没恢复的话，读到的就是解锁用的值：恢复成 WDA 默认值
    let restore = if saved == UNLOCK_IDLE { WDA_DEFAULT_IDLE } else { saved };
    wda.set_idle_settings(UNLOCK_IDLE).await?;
    let r = unlock(wda, cfg, code.as_bytes()).await;
    if let Err(e) = wda.set_idle_settings(restore).await {
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

async fn press<T: Transport>(wda: &mut WdaClient<T>, at: (i64, i64), t: &Timing) -> Result<()> {
    Ok(wda.actions(&tap_steps(at.0 as f64, at.1 as f64, t.key_press_ms)).await?)
}

/// 用删除键把 `entered` 位残留删到 0；找不到删除键或某次没删掉返回 false（这轮作废，一位没输）
async fn clear_residual<T: Transport>(
    wda: &mut WdaClient<T>,
    delete: Option<(i64, i64)>,
    entered: u32,
    t: &Timing,
) -> Result<bool> {
    let Some(del) = delete else { return Ok(false) };
    for left in (0..entered).rev() {
        if tap_key(wda, del, left as i64 + 1, t).await? != left as i64 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 按一下键（数字或删除），等密码栏已输入位数离开 `before`；返回最后读到的位数（读不到密码栏为 -1）。
/// 只认「变了」不认方向：删除键是变少，数字键是变多，按键被记成两下时会多变一位
async fn tap_key<T: Transport>(wda: &mut WdaClient<T>, at: (i64, i64), before: i64, t: &Timing) -> Result<i64> {
    press(wda, at, t).await?;
    let deadline = tokio::time::Instant::now() + t.key_confirm;
    loop {
        let got = wda.passcode_field().await?.map(|f| f.1 as i64).unwrap_or(-1);
        // 读不到密码栏可能只是动画中，等到超时再算
        if (got >= 0 && got != before) || tokio::time::Instant::now() >= deadline {
            return Ok(got);
        }
        tokio::time::sleep(t.poll.min(Duration::from_millis(150))).await;
    }
}

/// 第 2 位起输入的结果
enum Verdict {
    Unlocked,
    /// 确定没有凑满整串（第 n 位被吞，或多记了一下但没满）
    NotSubmitted(u32),
    /// 凑满整串后仍锁着
    Wrong,
}

fn write_flag(cfg: &UnlockConfig, why: &str) -> std::io::Result<()> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    std::fs::write(&cfg.disabled_flag, format!("{now} {why}\n"))
}

async fn unlock<T: Transport>(wda: &mut WdaClient<T>, cfg: &UnlockConfig, code: &[u8]) -> Result<Outcome> {
    let t = &cfg.timing;
    let screen = wda.screen_size().await?;
    let digit = |i: usize| (code[i] - b'0') as usize;
    let len = code.len() as u32;

    let mut keys = None;
    for _ in 0..FIRST_DIGIT_ROUNDS {
        // Home 唤醒（键盘已打开时 iOS 27 只是把键收起，半截输入还在，下面用删除键处理）
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
        let check = wda.check_lock(None, false).await?;
        if !check.locked {
            return Ok(Outcome::Unlocked);
        }
        // 核对时已取到元素树就直接用，密码栏也从树里读。那棵树是上滑后马上取的，键盘有时还没进树：
        // 缺键盘或密码栏时再取一次（真机上这样省掉一整轮，约 4.5 秒）
        let mut els = match &check.tree {
            Some(xml) => elements::parse(xml, screen),
            None => wda.elements().await?,
        };
        if check.tree.is_some() && (elements::keypad(&els).is_none() || elements::passcode_field(&els).is_none()) {
            els = wda.elements().await?;
        }
        let field = match elements::passcode_field(&els) {
            Some(f) => Some(f),
            None => wda.passcode_field().await?,
        };
        let (Some(pad), Some((total, entered))) = (elements::keypad(&els), field) else {
            continue;
        };
        if total != len {
            return Err(UnlockError::LengthMismatch { expected: total, configured: len });
        }
        // 有残留输入：iOS 27 上 Home 不会清空，用右下角「删除」逐位删掉，每删一位核对一次
        if entered != 0 && !clear_residual(wda, elements::delete_key(&els, &pad), entered, t).await? {
            continue;
        }
        // 第一位不可能凑满整串（至少 4 位），出什么情况都不算输错
        match tap_key(wda, pad[digit(0)], 0, t).await? {
            1 => {
                keys = Some(pad);
                break;
            }
            n if n > 1 => return Err(UnlockError::Incomplete(1)),
            _ => continue,
        }
    }
    let pad = keys.ok_or(UnlockError::NoKeypad)?;

    // 从第 2 位起，任何一下都可能凑满整串让 iOS 判定一次（按键被记成两下、最后一位）。先写停用标记，
    // 确认解开或确认没凑满才撤掉；中途出错、进程被杀都会留下标记，绝不会把同一个错误密码再输一遍
    write_flag(cfg, "正在输入密码，未确认结果（若长期存在：上次输入中途出错或进程退出）")?;
    let verdict = enter_rest(wda, cfg, &pad, code).await;
    let clear = || {
        if let Err(e) = std::fs::remove_file(&cfg.disabled_flag) {
            tracing::warn!("撤掉停用标记失败，自动解锁保持停用：{e}");
        }
    };
    match verdict {
        Ok(Verdict::Unlocked) => {
            clear();
            Ok(Outcome::Unlocked)
        }
        Ok(Verdict::NotSubmitted(n)) => {
            clear();
            Err(UnlockError::Incomplete(n))
        }
        Ok(Verdict::Wrong) => {
            let _ = write_flag(cfg, "整串密码输完仍是锁屏状态");
            Err(UnlockError::WrongPasscode)
        }
        Err(e) => {
            let why = format!("输入过程中出错，无法确认密码是否已提交：{e}");
            let _ = write_flag(cfg, &why);
            Err(UnlockError::Uncertain(e.to_string()))
        }
    }
}

/// 输第 2 位到最后一位并确认结果。出错直接返回（调用方保留停用标记）
async fn enter_rest<T: Transport>(
    wda: &mut WdaClient<T>,
    cfg: &UnlockConfig,
    pad: &[(i64, i64); 10],
    code: &[u8],
) -> Result<Verdict> {
    let t = &cfg.timing;
    let len = code.len() as u32;
    let key = |i: usize| pad[(code[i] - b'0') as usize];
    let mut last_pressed = true;
    for n in 2..len {
        let got = tap_key(wda, key(n as usize - 1), n as i64 - 1, t).await?;
        if got == n as i64 {
            continue;
        }
        // 被吞（还是 n-1 位）或多记一下但没满：确定没提交
        if got == n as i64 - 1 || (got > n as i64 && got < len as i64) {
            return Ok(Verdict::NotSubmitted(n));
        }
        // 归零、密码栏消失等：可能已经凑满提交了，按提交处理去确认
        tracing::warn!("第 {n} 位按后密码栏变成 {got} 位，可能已提交，确认结果");
        last_pressed = false;
        break;
    }
    if last_pressed {
        press(wda, key(code.len() - 1), t).await?;
    }
    let deadline = tokio::time::Instant::now() + t.unlock_confirm;
    loop {
        tokio::time::sleep(t.poll).await;
        // 严格判断：核对出错一律当仍锁着继续等，宁可多停用一次也不把错误密码当成解开
        if let Ok(c) = wda.check_lock(None, true).await
            && !c.locked
        {
            return Ok(Verdict::Unlocked);
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    // 仍锁着。最后一位被吞时密码栏停在 len-1 位，说明根本没提交，不算输错
    if last_pressed && matches!(wda.passcode_field().await, Ok(Some((_, e))) if e == len - 1) {
        return Ok(Verdict::NotSubmitted(len));
    }
    Ok(Verdict::Wrong)
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

    /// 键盘上留着别人按的半截（可能是错的）：先删干净再输，不能接在后面凑成一次输错
    #[tokio::test]
    async fn residual_input_is_deleted_before_typing() {
        let fake = FakeLockScreen::new("123456");
        fake.leave_residual(&[9, 9]);
        let (cfg, dir) = cfg(Some("123456"));
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::Unlocked);
        assert_eq!(fake.state().failed_attempts, 0);
        assert!(!cfg.disabled_flag.exists());
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

    /// 最后一位被吞：没提交，不算输错，也不停用
    #[tokio::test]
    async fn dropped_last_digit_is_not_a_wrong_attempt() {
        let fake = FakeLockScreen::new("123456");
        fake.drop_press(6);
        let (cfg, dir) = cfg(Some("123456"));
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await, Err(UnlockError::Incomplete(6))));
        assert_eq!(fake.state().failed_attempts, 0);
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 某一下被记成两下、凑满了整串：iOS 判了一次，必须当作输错停用
    #[tokio::test]
    async fn double_press_that_fills_the_entry_disables() {
        let fake = FakeLockScreen::new("123456");
        fake.double_press(5);
        let (cfg, dir) = cfg(Some("123456"));
        let mut wda = WdaClient::new(&fake);
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::WrongPasscode)));
        assert_eq!(fake.state().failed_attempts, 1);
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::Disabled)));
        assert_eq!(fake.state().failed_attempts, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 第 2 位之后出错：不知道是否提交了，停用标记必须留着，下一次不能再输
    #[tokio::test]
    async fn error_after_second_digit_keeps_auto_unlock_disabled() {
        let fake = FakeLockScreen::new("123456");
        fake.broken_after_press(3);
        let (cfg, dir) = cfg(Some("123456"));
        let mut wda = WdaClient::new(&fake);
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::Uncertain(_))));
        assert!(cfg.disabled_flag.exists());
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::Disabled)));
        assert_eq!(fake.state().failed_attempts, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// WDA 在锁屏时启动（/wda/locked 一直报 false），输错后取元素树又失败：
    /// 确认阶段不能按 WDA 的 false 当成解开了
    #[tokio::test]
    async fn stale_wda_lock_state_cannot_turn_a_wrong_entry_into_unlocked() {
        let fake = FakeLockScreen::new("123456");
        fake.stale_wda_lock();
        fake.broken_after_press(6);
        let (cfg, dir) = cfg(Some("654321"));
        let mut wda = WdaClient::new(&fake);
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::WrongPasscode)));
        assert_eq!(fake.state().failed_attempts, 1);
        assert!(cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 上次异常退出没恢复等待设置：恢复成 WDA 默认值，而不是把解锁用的值当成原值
    #[tokio::test]
    async fn stuck_unlock_idle_settings_are_restored_to_defaults() {
        let fake = FakeLockScreen::new("123456");
        let (cfg, dir) = cfg(Some("123456"));
        let mut wda = WdaClient::new(&fake);
        wda.set_idle_settings(UNLOCK_IDLE).await.unwrap();
        assert_eq!(ensure_unlocked(&mut wda, &cfg).await.unwrap(), Outcome::Unlocked);
        assert_eq!(fake.state().idle, (10.0, 2.0));
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
