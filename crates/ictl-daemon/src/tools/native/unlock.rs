//! Native lock-state checks and the `ictl_unlock::Screen` adapter.

use ictl_ui::View;
use ictl_unlock::{LockCheck, Screen, ScreenError, ScreenResult};

use super::super::*;

/// After Home, how long the screen takes to wake before the swipe up (WDA's 1 s covers its slow queries)
const NATIVE_WAKE_SETTLE: Duration = Duration::from_millis(500);
/// After unlocking, the lock screen slides away for about half a second
const UNLOCK_SETTLE: Duration = Duration::from_millis(700);

impl Daemon {
    /// Whether the phone is locked (lockdown's `PasswordProtected`)
    pub(crate) async fn native_locked(&self, g: &mut Inner) -> crate::error::Result<bool> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        Ok(device.passcode_locked().await?)
    }

    /// Unlocks natively, with the same rules and guards as through WDA (`ictl_unlock`)
    pub(crate) async fn native_unlock(
        &self,
        g: &mut Inner,
        cfg: &ictl_unlock::UnlockConfig,
    ) -> ictl_unlock::Result<ictl_unlock::Outcome> {
        // Home wakes a dark screen within about 0.3 s; the keypad is looked for as it comes (`reads_cheaply`)
        let cfg = ictl_unlock::UnlockConfig {
            passcode_file: cfg.passcode_file.clone(),
            disabled: cfg.disabled.clone(),
            timing: ictl_unlock::Timing { wake_settle: NATIVE_WAKE_SETTLE, ..cfg.timing },
        };
        let mut screen = NativeScreen { d: self, g };
        let r = Box::pin(ictl_unlock::ensure_unlocked(&mut screen, &cfg)).await;
        if matches!(r, Ok(ictl_unlock::Outcome::Unlocked)) {
            // the lock screen is still sliding away: a tap now is lost (a tap by text right after unlocking did
            // nothing on the phone)
            tokio::time::sleep(UNLOCK_SETTLE).await;
        }
        r
    }
}

/// The phone as [`ictl_unlock`] sees it, driven natively
struct NativeScreen<'a> {
    d: &'a Daemon,
    g: &'a mut Inner,
}

/// Reads the lock screen: with the HID session's screen stream open, the phone hides the lock screen and its
/// passcode keypad from accessibility (one element, the app's, for every session; iPhone 15 Pro Max, iOS 27.2.1), so
/// the session is closed first, and the elements come back a moment after. The next tap opens it again (about 0.5 s).
/// Not before the last touch has landed ([`TOUCH_LANDS`])
async fn read_lock_screen(d: &Daemon, g: &mut Inner) -> crate::error::Result<(elements::Source, ScreenSize, i64)> {
    let was_open = g.tunnel.as_ref().is_some_and(ictl_device::Tunnel::hid_open);
    if was_open && let Some(left) = touch_lands_in(g.hid_used.map(|t| t.elapsed())) {
        tokio::time::sleep(left).await;
    }
    crate::core::close_hid(g).await;
    let t0 = Instant::now();
    loop {
        let read = Box::pin(d.ax_read(g, Duration::from_secs(3))).await?;
        let shown = read.0.nodes().nth(1).is_some();
        if shown || !was_open || t0.elapsed() >= LOCK_SCREEN_BACK {
            tracing::debug!(
                "lock screen read {:.0} ms after closing HID, elements shown: {shown}",
                t0.elapsed().as_secs_f64() * 1000.0
            );
            return Ok(read);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// A touch sent over HID doesn't count when its session closes right after it: the passcode keypad came up, but no
/// key went down and the field stayed at 0 digits, so unlocking failed with "couldn't bring up the keypad" (or
/// stopped at the 2nd or 3rd digit). Not every time: the same phone unlocked with no wait for hours, then lost
/// every key. Unlocks while it lost them, by how long the session stayed open after the touch (iPhone19,7,
/// iOS 27.2.1, one daemon, the waits taken in turn): 0 ms 0 of 9, 50 ms 0 of 3, 100 ms 0 of 3, 150 ms 1 of 3,
/// 200 ms 3 of 4, 250 ms 5 of 5, 350 ms 3 of 3. An unlock takes about 1.5 s longer for it (8.3 s without)
pub(super) const TOUCH_LANDS: Duration = Duration::from_millis(350);

/// How much longer the HID session has to stay open for the last touch sent over it to count, `since` it was sent
pub(super) fn touch_lands_in(since: Option<Duration>) -> Option<Duration> {
    TOUCH_LANDS.checked_sub(since?).filter(|d| !d.is_zero())
}

/// How long after the HID session closes the lock screen's elements may take to come back
const LOCK_SCREEN_BACK: Duration = Duration::from_secs(3);

/// A step that failed: unlocking tries another round
fn step(e: impl std::error::Error + Send + Sync + 'static) -> ScreenError {
    ScreenError::Step(Box::new(e))
}

/// The phone can't be looked at: unlocking takes it for not locked, the next call says why
fn unavailable(e: impl std::error::Error + Send + Sync + 'static) -> ScreenError {
    ScreenError::Unavailable(Box::new(e))
}

impl Screen for NativeScreen<'_> {
    // nothing to set: no waits as WDA's
    type Waits = ();

    async fn reports_locked(&mut self) -> ScreenResult<bool> {
        self.d.native_locked(self.g).await.map_err(unavailable)
    }

    fn lock_state_trusted(&self) -> bool {
        // lockdown knows: no lock-screen-looking notification center to tell apart
        true
    }

    async fn check_lock(&mut self, _reported: Option<bool>, _strict: bool) -> ScreenResult<LockCheck> {
        let locked = self.d.native_locked(self.g).await.map_err(unavailable)?;
        let tree = if locked { read_lock_screen(self.d, self.g).await.ok().map(|(src, _, _)| src) } else { None };
        Ok(LockCheck { locked, uncertain: false, tree })
    }

    async fn quick_waits(&mut self) -> ScreenResult<Option<()>> {
        Ok(None)
    }

    async fn restore_waits(&mut self, _: ()) -> ScreenResult<()> {
        Ok(())
    }

    async fn press_home(&mut self, _round: u32) -> ScreenResult<()> {
        self.d.hid_do(self.g, |h| Box::pin(h.press(Button::Home))).await.map_err(step)
    }

    async fn view(&mut self) -> ScreenResult<View> {
        self.d.screen(self.g).await.map(View::from).map_err(unavailable)
    }

    async fn swipe(&mut self, from: (f64, f64), to: (f64, f64), ms: u64) -> ScreenResult<()> {
        let rot = self.d.hid_touch_rotation(self.g).await.map_err(step)?;
        let seconds = ms as f64 / 1000.0;
        self.d
            .hid_do(self.g, move |h| {
                Box::pin(async move {
                    h.rotation = rot;
                    h.drag(from, to, seconds).await
                })
            })
            .await
            .map_err(step)
    }

    async fn tap(&mut self, x: f64, y: f64, _press_ms: u64) -> ScreenResult<()> {
        self.d.native_tap(self.g, x, y).await.map_err(step)
    }

    async fn elements(&mut self) -> ScreenResult<Vec<elements::Element>> {
        let (src, size, _) = read_lock_screen(self.d, self.g).await.map_err(step)?;
        Ok(Tree::of(&src, size).elements)
    }

    async fn passcode_field(&mut self) -> ScreenResult<Option<(u32, u32)>> {
        Ok(elements::passcode_field(&self.elements().await?))
    }

    fn reads_cheaply(&self) -> bool {
        // a read of the lock screen takes about 0.1 s
        true
    }
}
