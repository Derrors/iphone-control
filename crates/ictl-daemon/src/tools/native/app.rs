//! Native app lifecycle (launch, terminate) and URL scheme resolution.

use super::super::*;
use super::AX_WAIT;

/// How long an app may take to come to the front
const LAUNCH_WAIT: Duration = Duration::from_secs(8);
/// How long a killed app's processes may take to be gone
const TERMINATE_WAIT: Duration = Duration::from_secs(2);

/// The app's zoom-in takes about half a second; a tap during it is lost (the same as WDA's `LAUNCH_MIN`)
const LAUNCH_SETTLE: Duration = Duration::from_millis(800);

impl Daemon {
    /// Launches `bundle_id` in front (with `url`, the link for it to open) and waits for it to be there
    pub(crate) async fn native_launch(
        &self,
        g: &mut Inner,
        bundle_id: &str,
        url: Option<&str>,
    ) -> crate::error::Result<ictl_wda::Launch> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let t0 = Instant::now();
        self.ensure_tunnel(g, &device).await?.launch_foreground(bundle_id, url).await?;
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let (src, size, pid) = self.ax_read(g, AX_WAIT).await?;
            let tree = Tree::of(&src, size);
            if let Some(a) = tree.alert().filter(|a| !a.label.is_empty()) {
                return Ok(ictl_wda::Launch::Blocked(a.label.clone()));
            }
            if self.bundle_of_pid(g, pid).await.is_ok_and(|b| b == bundle_id) {
                if let Some(left) = LAUNCH_SETTLE.checked_sub(t0.elapsed()) {
                    tokio::time::sleep(left).await;
                }
                return Ok(ictl_wda::Launch::Foreground);
            }
            if t0.elapsed() >= LAUNCH_WAIT {
                return Ok(ictl_wda::Launch::Slow);
            }
        }
    }

    /// Ends `bundle_id`'s process (SIGKILL through CoreDevice); Ok(false) when it wasn't running
    pub(crate) async fn native_terminate(&self, g: &mut Inner, bundle_id: &str) -> crate::error::Result<bool> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let procs = self.ensure_tunnel(g, &device).await?.processes().await?;
        let mut apps = self.list_apps(g, false).await?;
        apps.extend(self.list_apps(g, true).await?);
        let pids: Vec<u64> = procs
            .iter()
            .filter(|(_, exe)| apps::bundle_of_process(exe, &apps).as_deref() == Some(bundle_id))
            .map(|(p, _)| *p)
            .collect();
        for pid in &pids {
            self.ensure_tunnel(g, &device).await?.signal(*pid, 9).await?;
            // the frontmost-app cache must not name a pid that may be reused
            g.dev.bundles.remove(*pid as i64);
        }
        // gone before this says so: a launch right after one killed failed with «The process identifier of the
        // launched application could not be determined. It may have already terminated.»
        let deadline = Instant::now() + TERMINATE_WAIT;
        while !pids.is_empty() && Instant::now() < deadline {
            let procs = self.ensure_tunnel(g, &device).await?.processes().await?;
            if !procs.iter().any(|(p, _)| pids.contains(p)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(!pids.is_empty())
    }

    /// The app that opens `url`
    pub(crate) async fn native_app_for_url(&self, g: &mut Inner, url: &str) -> crate::error::Result<String> {
        let mut apps = self.list_apps(g, false).await?;
        apps.extend(self.list_apps(g, true).await?);
        apps::app_for_url(url, &apps)
            .ok_or_else(|| ictl_l10n::trf!("手机上没有 App 能打开 {url}", "No app on the phone opens {url}").into())
    }
}
