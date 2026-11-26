//! [`Screen`] through WebDriverAgent. Its errors: unavailable is [`ScreenError::Unavailable`], a step WDA refused is
//! [`ScreenError::Step`], the rest [`ScreenError::Other`]

use ictl_wda::{IdleSettings, WdaClient, WdaError, http::Transport, swipe_steps, tap_steps};

use super::*;

impl From<WdaError> for ScreenError {
    fn from(e: WdaError) -> Self {
        match e {
            WdaError::Unavailable(_) => Self::Unavailable(Box::new(e)),
            WdaError::Remote { .. } => Self::Step(Box::new(e)),
            e => Self::Other(Box::new(e)),
        }
    }
}

impl From<ictl_wda::client::LockCheck> for LockCheck {
    fn from(c: ictl_wda::client::LockCheck) -> Self {
        Self { locked: c.locked, uncertain: c.uncertain, tree: c.tree }
    }
}

impl<T: Transport> Screen for WdaClient<T> {
    type Waits = IdleSettings;

    async fn reports_locked(&mut self) -> ScreenResult<bool> {
        Ok(self.wda_reports_locked().await?)
    }
    fn lock_state_trusted(&self) -> bool {
        WdaClient::lock_state_trusted(self)
    }
    async fn check_lock(&mut self, reported: Option<bool>, strict: bool) -> ScreenResult<LockCheck> {
        Ok(WdaClient::check_lock(self, reported, strict).await?.into())
    }
    async fn quick_waits(&mut self) -> ScreenResult<Option<IdleSettings>> {
        // 解锁期间等待上限调小（不能是 0：唤醒会失败），做完恢复成平时的值。
        // 上次异常退出没恢复的话读到的是调小的值，baseline 会按 WDA 默认值算
        let restore = self.baseline().await?;
        self.set_idle_settings(IdleSettings::QUICK).await?;
        Ok(Some(restore))
    }
    async fn restore_waits(&mut self, to: IdleSettings) -> ScreenResult<()> {
        Ok(self.set_idle_settings(to).await?)
    }
    async fn press_home(&mut self, round: u32) -> ScreenResult<()> {
        // XCTest 的 home 和 IOHID 的 Home 轮流用：iPhone 12 mini（iOS 26.4.1）上 XCTest 的 home 点不亮熄灭的屏幕，
        // IOHID 的能；iOS 27 上一直用的是 XCTest 的，第一轮还是它
        if round.is_multiple_of(2) {
            self.press_button("home").await?
        } else {
            self.press_home_hid().await?
        }
        Ok(())
    }
    async fn view(&mut self) -> ScreenResult<elements::View> {
        Ok(WdaClient::view(self).await?)
    }
    async fn swipe(&mut self, from: (f64, f64), to: (f64, f64), ms: u64) -> ScreenResult<()> {
        Ok(self.actions(&swipe_steps(from, to, ms)).await?)
    }
    async fn tap(&mut self, x: f64, y: f64, press_ms: u64) -> ScreenResult<()> {
        Ok(self.actions(&tap_steps(x, y, press_ms)).await?)
    }
    async fn elements(&mut self) -> ScreenResult<Vec<elements::Element>> {
        Ok(WdaClient::elements(self).await?)
    }
    async fn passcode_field(&mut self) -> ScreenResult<Option<(u32, u32)>> {
        Ok(WdaClient::passcode_field(self).await?)
    }
}
