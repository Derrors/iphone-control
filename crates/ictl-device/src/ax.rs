//! The screen's elements through remoteAXService ([`ictl_ax`]), without WebDriverAgent: the session lives on the
//! tunnel like the HID session, and is opened again when the phone closed it.

use std::time::{Duration, Instant};

use ictl_ax::{Element, ObjectRef};
use idevice::ReadWrite;

use crate::{DeviceError, Result, Tunnel, limit};

pub type AxSession = ictl_ax::Session<Box<dyn ReadWrite>>;

/// The handshake answers in tens of milliseconds over USB
const CONNECT_WAIT: Duration = Duration::from_secs(8);
/// A first tree after a change lacks the element types; the complete one follows within about half a second
const COMPLETE_WAIT: Duration = Duration::from_millis(900);
/// At most this many of SpringBoard's elements are read over the app (the status bar and what covers the app)
const OVERLAY_LIMIT: usize = 80;

impl From<ictl_ax::AxError> for DeviceError {
    fn from(e: ictl_ax::AxError) -> Self {
        match e {
            ictl_ax::AxError::Timeout(what) => Self::Timeout(format!("remoteAXService: {what}")),
            other => Self::Idevice(other.to_string()),
        }
    }
}

/// One read of the screen
#[derive(Debug, Clone)]
pub struct AxScreen {
    /// The frontmost application's tree, its element first
    pub app: Vec<Element>,
    /// What SpringBoard shows over the app (alerts, banners, its status bar); empty when SpringBoard is the app
    pub overlays: Vec<Element>,
    pub app_pid: i64,
    /// When the phone pushed the tree
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
            let port = self
                .handshake
                .services
                .get(ictl_ax::SERVICE)
                .map(|s| s.port)
                .ok_or_else(|| DeviceError::Idevice(format!("the phone offers no {}", ictl_ax::SERVICE)))?;
            let stream: Box<dyn ReadWrite> = Box::new(self.adapter.connect(port).await?);
            let s =
                limit(ictl_l10n::tr!("连接无障碍服务", "connect to the accessibility service"), CONNECT_WAIT, async {
                    Ok(AxSession::connect(stream, CONNECT_WAIT).await?)
                })
                .await?;
            self.ax = Some(s);
        }
        Ok(self.ax.as_ref().expect("just opened"))
    }

    /// Closes the session (the phone turns its accessibility runtime off for us)
    pub async fn close_ax(&mut self) {
        if let Some(s) = self.ax.take() {
            s.close().await;
        }
    }

    /// The screen's elements: the last tree the phone pushed, or, with `after`, the first one pushed after it
    /// (waiting up to `wait`); and SpringBoard's elements over the app
    pub async fn ax_screen(&mut self, after: Option<Instant>, wait: Duration) -> Result<AxScreen> {
        let sb = self.springboard_pid().await;
        let s = self.ax().await?;
        let pushed = match after {
            Some(t) => s.wait_push(t, wait).await,
            None => match s.latest() {
                // a first tree: give the complete one a moment
                Some(p) if p.tree.initial => s.wait_push(p.at, COMPLETE_WAIT).await.or(Some(p)),
                Some(p) => Some(p),
                // a session opened just now: the phone pushes the screen within about a second
                None => {
                    let long_ago = Instant::now().checked_sub(Duration::from_secs(3600)).unwrap_or_else(Instant::now);
                    s.wait_push(long_ago, wait).await
                }
            },
        };
        let p = pushed.ok_or(DeviceError::Timeout(ictl_l10n::trf!(
            "{} 秒内没有收到界面元素",
            "no screen elements within {} s",
            wait.as_secs_f64()
        )))?;
        let app_pid = p.tree.app_pid();
        let mut overlays = Vec::new();
        if let Some(sb) = sb.filter(|sb| *sb != app_pid) {
            match springboard_overlays(s, sb).await {
                Ok(o) => overlays = o,
                Err(e) => tracing::debug!("SpringBoard's elements: {e}"),
            }
        }
        Ok(AxScreen { app: p.tree.elements.clone(), overlays, app_pid, at: p.at })
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

/// SpringBoard's application element answers whichever app is in front: its children are the status bar's items
/// and whatever it draws over the app (Pinky, DIYgod/pinky `springboard.go`)
async fn springboard_overlays(s: &AxSession, pid: i64) -> Result<Vec<Element>> {
    let app: ObjectRef = s.app_object(pid).await?;
    Ok(s.walk_children(app, OVERLAY_LIMIT, ictl_ax::tree::trait_bit::STATUS_BAR).await?)
}
