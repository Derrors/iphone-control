//! The screen's elements through remoteAXService ([`ictl_ax`]), without WebDriverAgent: the session lives on the
//! tunnel like the HID session, and is opened again when the phone closed it.

use std::time::{Duration, Instant};

use ictl_ax::{Element, ObjectRef};
use idevice::ReadWrite;

use crate::{DeviceError, Result, Tunnel, limit};

pub type AxSession = ictl_ax::Session<Box<dyn ReadWrite>>;

/// The handshake answers in tens of milliseconds over USB
const CONNECT_WAIT: Duration = Duration::from_secs(8);
/// A walk of the app reads at most this many elements, within this long (Pinky's bounds over USB)
const WALK_LIMIT: usize = 400;
const WALK_BUDGET: Duration = Duration::from_secs(4);
/// At most this many of SpringBoard's elements are read over the app (the status bar and what covers the app)
const OVERLAY_LIMIT: usize = 80;
const OVERLAY_BUDGET: Duration = Duration::from_secs(2);

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

    /// The screen's elements as they are now: a walk of the frontmost app's tree, and SpringBoard's elements over
    /// the app. `wait` bounds a session's opening and the first request (an app that just came up builds its
    /// accessibility tree first)
    pub async fn ax_screen(&mut self, wait: Duration) -> Result<AxScreen> {
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
        let s = self.ax().await?;
        if !complete {
            tracing::debug!("remoteAXService: walk cut at {} elements", elements.len());
        }
        if elements.is_empty() {
            return Err(DeviceError::Timeout(ictl_l10n::tr!("没有读到界面元素", "no screen elements read").into()));
        }
        let mut overlays = Vec::new();
        if let Some(sb) = sb.filter(|sb| *sb != app.pid) {
            match springboard_overlays(s, sb).await {
                Ok(o) => overlays = o,
                Err(e) => tracing::debug!("SpringBoard's elements: {e}"),
            }
        }
        Ok(AxScreen {
            app: elements,
            overlays,
            app_pid: app.pid,
            springboard_front: sb == Some(app.pid),
            at: Instant::now(),
        })
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

/// SpringBoard's application element answers whichever app is in front: its children are the status bar's items
/// and whatever it draws over the app (Pinky, DIYgod/pinky `springboard.go`)
async fn springboard_overlays(s: &AxSession, pid: i64) -> Result<Vec<Element>> {
    let app: ObjectRef = s.app_object(pid).await?;
    Ok(s.walk_children(app, OVERLAY_LIMIT, OVERLAY_BUDGET, ictl_ax::tree::trait_bit::STATUS_BAR).await?)
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
