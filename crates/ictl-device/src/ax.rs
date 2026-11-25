//! The screen's elements through remoteAXService ([`ictl_ax`]), without WebDriverAgent: the session lives on the
//! tunnel like the HID session, and is opened again when the phone closed it.

use std::time::{Duration, Instant};

use ictl_ax::{Element, ObjectRef};
use idevice::ReadWrite;

use crate::{DeviceError, Result, Tunnel, limit};

pub type AxSession = ictl_ax::Session<Box<dyn ReadWrite>>;

/// The handshake answers in tens of milliseconds over USB
const CONNECT_WAIT: Duration = Duration::from_secs(8);
/// A walk of the app reads at most this many elements, within this long
const WALK_LIMIT: usize = 400;
const WALK_BUDGET: Duration = Duration::from_secs(4);
/// At most this many of SpringBoard's elements are read over the app (the status bar and what covers the app)
const OVERLAY_LIMIT: usize = 80;
const OVERLAY_BUDGET: Duration = Duration::from_secs(2);

impl From<ictl_ax::AxError> for DeviceError {
    fn from(e: ictl_ax::AxError) -> Self {
        match e {
            ictl_ax::AxError::Timeout(what) => Self::AxUnanswered(format!("remoteAXService: {what}")),
            other => Self::Idevice(other.to_string()),
        }
    }
}

/// After axauditd is ended, how long launchd is given before the next connection starts it again
const AX_DAEMON_RESTART: Duration = Duration::from_secs(1);

/// The phone's process behind remoteAXService
pub fn is_ax_daemon(exe: &str) -> bool {
    exe.ends_with("/AccessibilityAudit.framework/Support/axauditd")
}

/// One read of the screen
#[derive(Debug, Clone)]
pub struct AxScreen {
    /// The frontmost application's tree, its element first
    pub app: Vec<Element>,
    /// Whether the walk reached the end rather than a node or time limit.
    pub app_complete: bool,
    /// What SpringBoard shows over the app (alerts, banners, its status bar); empty when SpringBoard is the app
    pub overlays: Vec<Element>,
    pub overlays_complete: bool,
    pub app_pid: i64,
    /// SpringBoard is the app in front (the home screen, the lock screen, or a question it asks over an app)
    pub springboard_front: bool,
    /// When the screen was read
    pub at: Instant,
}

impl Tunnel {
    /// Whether the phone offers remoteAXService (iOS 27 with the developer disk image)
    pub fn ax_supported(&self) -> bool {
        self.handshake.services.contains_key(ictl_ax::SERVICE)
    }

    pub fn ax_open(&self) -> bool {
        self.ax.as_ref().is_some_and(|s| !s.is_closed())
    }

    /// The session, opened (again) if needed
    pub async fn ax(&mut self) -> Result<&AxSession> {
        if !self.ax_open() {
            self.ax = None;
            self.springboard_app = None;
            let port = self
                .handshake
                .services
                .get(ictl_ax::SERVICE)
                .map(|s| s.port)
                .ok_or_else(|| DeviceError::Idevice(format!("the phone offers no {}", ictl_ax::SERVICE)))?;
            // the tunnel's own connect has no time limit: one that doesn't come is the tunnel's fault (a timeout, the
            // tunnel is rebuilt), not the accessibility daemon's
            let adapter = &mut self.adapter;
            let stream: Box<dyn ReadWrite> = Box::new(
                limit(
                    ictl_l10n::tr!("连接无障碍服务的端口", "connect to the accessibility service's port"),
                    CONNECT_WAIT,
                    async { Ok(adapter.connect(port).await?) },
                )
                .await?,
            );
            let s =
                limit(ictl_l10n::tr!("连接无障碍服务", "connect to the accessibility service"), CONNECT_WAIT, async {
                    Ok(AxSession::connect(stream, CONNECT_WAIT).await?)
                })
                .await
                .map_err(|e| match e {
                    DeviceError::Timeout(what) => DeviceError::AxUnanswered(what),
                    e => e,
                })?;
            self.ax = Some(s);
        }
        Ok(self.ax.as_ref().expect("just opened"))
    }

    /// Closes the session (the phone turns its accessibility runtime off for us)
    pub async fn close_ax(&mut self) {
        self.springboard_app = None;
        if let Some(s) = self.ax.take() {
            s.close().await;
        }
    }

    /// The screen's elements as they are now: a walk of the frontmost app's tree, and (with `overlays`)
    /// SpringBoard's elements over the app. `wait` bounds a session's opening and the first request (an app that just came up builds its
    /// accessibility tree first)
    pub async fn ax_screen(&mut self, wait: Duration, overlays: bool) -> Result<AxScreen> {
        let sb = self.springboard_pid().await;
        let (mut app, (mut elements, mut complete)) = {
            let s = self.ax().await?;
            let app = frontmost(s, wait).await?;
            (app, s.walk(app, WALK_LIMIT, WALK_BUDGET, 0).await?)
        };
        // A session that lived through the phone locking reads the app's element alone from then on (the passcode
        // keypad up, a new session read 15 elements): read again on a new one
        if elements.len() <= 1 {
            tracing::debug!("remoteAXService: app {} reads empty, reconnecting", app.pid);
            self.close_ax().await;
            let s = self.ax().await?;
            app = frontmost(s, wait).await?;
            (elements, complete) = s.walk(app, WALK_LIMIT, WALK_BUDGET, 0).await?;
        }
        if !complete {
            tracing::debug!("remoteAXService: walk cut at {} elements", elements.len());
        }
        if elements.is_empty() {
            tracing::debug!("remoteAXService: app {} reads no elements on a new session either", app.pid);
            return Err(nothing_read());
        }
        let (overlays, overlays_complete) = if overlays { self.ax_overlays(app.pid).await } else { (Vec::new(), true) };
        Ok(AxScreen {
            app: elements,
            app_complete: complete,
            overlays,
            overlays_complete,
            app_pid: app.pid,
            springboard_front: sb == Some(app.pid),
            at: Instant::now(),
        })
    }

    /// What SpringBoard shows over the app of process `app_pid` (alerts, banners, its status bar); empty when
    /// SpringBoard is that app, or can't be read
    pub async fn ax_overlays(&mut self, app_pid: i64) -> (Vec<Element>, bool) {
        let Some(sb) = self.springboard_pid().await else { return (Vec::new(), false) };
        if sb == app_pid {
            return (Vec::new(), true);
        }
        let r = async {
            let root = match self.springboard_app {
                Some(o) => o,
                None => {
                    let o = self.ax().await?.app_object(sb).await?;
                    self.springboard_app = Some(o);
                    o
                }
            };
            let s = self.ax().await?;
            Ok::<_, DeviceError>(
                s.walk_children(root, OVERLAY_LIMIT, OVERLAY_BUDGET, ictl_ax::tree::trait_bit::STATUS_BAR).await?,
            )
        }
        .await;
        r.unwrap_or_else(|e| {
            tracing::debug!("SpringBoard's elements: {e}");
            self.springboard_app = None;
            (Vec::new(), false)
        })
    }

    /// Ends axauditd, the phone's process behind remoteAXService; the next connection starts it again. It can get stuck
    /// (iPhone 15 Pro Max, iOS 27.2.1: after automatic unlocks and daemon restarts): every handshake then times out
    /// on `deviceAPIVersion`, a new session or tunnel doesn't help, and before this only replugging the phone did.
    /// Returns how many processes were ended (0: it wasn't running)
    pub async fn restart_ax_daemon(&mut self) -> Result<usize> {
        self.close_ax().await;
        let pids: Vec<u64> =
            self.processes().await?.into_iter().filter(|(_, exe)| is_ax_daemon(exe)).map(|(pid, _)| pid).collect();
        for pid in &pids {
            self.signal(*pid, 9).await?;
        }
        if !pids.is_empty() {
            tokio::time::sleep(AX_DAEMON_RESTART).await;
        }
        Ok(pids.len())
    }

    /// The frontmost app's process (one request, no walk)
    pub async fn ax_frontmost_pid(&mut self, wait: Duration) -> Result<i64> {
        let s = self.ax().await?;
        Ok(frontmost(s, wait).await?.pid)
    }

    /// SpringBoard's process, found once per tunnel
    async fn springboard_pid(&mut self) -> Option<i64> {
        if self.springboard.is_none() {
            let found = self
                .processes()
                .await
                .ok()?
                .into_iter()
                .find(|(_, exe)| exe.ends_with("/SpringBoard.app/SpringBoard"))
                .map(|(pid, _)| pid as i64);
            self.springboard = found;
        }
        self.springboard
    }
}

// SpringBoard's application element answers whichever app is in front: its children are the status bar's items
// and whatever it draws over the app; see `Tunnel::ax_overlays`

/// The frontmost app reads no elements at all, not even its own application element, on a new session too. axauditd
/// gets stuck this way as well (iPhone19,7, iOS 27.2.1): every app read empty, SpringBoard read as usual, with no
/// timeout, and neither a new session nor restarting the daemon helped; ending axauditd did. So it is told as the
/// service not answering ([`DeviceError::AxUnanswered`], read the same as before), which the daemon restarts it for.
/// Not to be confused with the lock screen under the HID screen stream (#126), which reads one element, the app's
fn nothing_read() -> DeviceError {
    DeviceError::AxUnanswered(ictl_l10n::tr!("没有读到界面元素", "no screen elements read").into())
}

/// The frontmost app's element, asked again for up to `wait` while the phone has none (between two apps)
async fn frontmost(s: &AxSession, wait: Duration) -> Result<ObjectRef> {
    let deadline = Instant::now() + wait;
    loop {
        match s.frontmost_app().await {
            Ok(app) => return Ok(app),
            Err(ictl_ax::AxError::Protocol(e)) if Instant::now() < deadline => {
                tracing::debug!("remoteAXService: no frontmost app yet: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod wedge_tests {
    use super::*;

    /// remoteAXService not answering is an error of its own (the daemon restarts axauditd for it), worded as before
    #[test]
    fn an_unanswered_accessibility_service_is_told_apart_and_reads_as_a_timeout() {
        let e = DeviceError::from(ictl_ax::AxError::Timeout("deviceAPIVersion"));
        assert!(matches!(&e, DeviceError::AxUnanswered(w) if w == "remoteAXService: deviceAPIVersion"), "{e:?}");
        assert_eq!(e.to_string(), DeviceError::Timeout("remoteAXService: deviceAPIVersion".into()).to_string());
    }

    /// An app that reads no elements at all is axauditd stuck too (restarted for), and says what it said before
    #[test]
    fn an_app_read_with_no_elements_is_an_unanswered_accessibility_service() {
        let e = nothing_read();
        assert!(matches!(&e, DeviceError::AxUnanswered(_)), "{e:?}");
        let before = DeviceError::Timeout(ictl_l10n::tr!("没有读到界面元素", "no screen elements read").into());
        assert_eq!(e.to_string(), before.to_string());
    }

    /// Paths as the phone lists them (iOS 27.2.1); only axauditd itself is ended
    #[test]
    fn only_axauditd_is_the_accessibility_daemon() {
        assert!(is_ax_daemon("file:///System/Library/PrivateFrameworks/AccessibilityAudit.framework/Support/axauditd"));
        for other in [
            "file:///System/Library/PrivateFrameworks/AXAssetLoader.framework/Support/axassetsd",
            "file:///System/Library/CoreServices/AccessibilityUIServer.app/AccessibilityUIServer",
            "file:///System/Library/Frameworks/MediaAccessibility.framework/XPCServices/com.apple.accessibility.mediaaccessibilityd.xpc/com.apple.accessibility.mediaaccessibilityd",
        ] {
            assert!(!is_ax_daemon(other), "{other}");
        }
    }
}
