//! `setup`'s device steps, done by the instance holding the device (docs/dev/multi-instance-plan.md 4.5): pairing
//! and Developer Mode ([`crate::protocol::Request::SetupStep`]).
//!
//! The checks and the step are made with the selection latch held, so no task is opened and no switch made between
//! them: the device must be the one this instance holds the lease of, and no task may be active.

use crate::core::Daemon;
use crate::protocol::SetupStepKind;

impl Daemon {
    pub(crate) async fn setup_step(&self, udid: &str, step: SetupStepKind) -> crate::error::Result<String> {
        let _latch = self.select_latch.lock().await;
        if let Some(t) = self.tasks.active() {
            drop(t);
            return Err(ictl_l10n::tr!(
                "有任务进行中，不做设置步骤；先停止任务",
                "A task is active, so the setup step wasn't done; stop the task first"
            )
            .into());
        }
        self.gate.check_open()?;
        let g = self.inner.lock().await;
        if !g.lease.as_ref().is_some_and(|l| l.is_for(udid)) {
            return Err(ictl_l10n::trf!(
                "这个实例没有在控制 {udid}，不能替它做设置",
                "This instance doesn't control {udid}, so it can't do its setup"
            )
            .into());
        }
        let device = g
            .device
            .clone()
            .filter(|d| d.udid.eq_ignore_ascii_case(udid))
            .ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        // the device lock held throughout: each step takes a moment
        Ok(match step {
            SetupStepKind::PairStatus => if device.paired().await? { "paired" } else { "unpaired" }.into(),
            SetupStepKind::Pair => match device.pair("iphone-control").await {
                Ok(()) => "paired".into(),
                Err(ictl_device::DeviceError::PairingPending | ictl_device::DeviceError::PasswordProtected) => {
                    "pending".into()
                }
                Err(e) => return Err(e.into()),
            },
            SetupStepKind::DevModeReveal => {
                device.reveal_developer_mode().await?;
                "revealed".into()
            }
            SetupStepKind::DevModeStatus => if device.developer_mode_enabled().await? { "on" } else { "off" }.into(),
        })
    }
}
